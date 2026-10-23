#!/usr/bin/env python3
"""Offline CPU embedding worker. Model code execution and remote loading are disabled."""
import hashlib
import json
import os
import struct
import sys
from pathlib import Path

MAX_FRAME = 1024 * 1024

def read_exact(count):
    data = bytearray()
    while len(data) < count:
        block = sys.stdin.buffer.read(count-len(data))
        if not block:
            raise EOFError()
        data.extend(block)
    return bytes(data)

def receive():
    size = struct.unpack('>I', read_exact(4))[0]
    if not 0 < size <= MAX_FRAME:
        raise ValueError('frame limit')
    return json.loads(read_exact(size))

def send(value):
    data = json.dumps(value, separators=(',', ':'), allow_nan=False).encode()
    if len(data) > MAX_FRAME:
        raise ValueError('frame limit')
    sys.stdout.buffer.write(struct.pack('>I', len(data))+data)
    sys.stdout.buffer.flush()

def main():
    os.environ.update(HF_HUB_OFFLINE='1', TRANSFORMERS_OFFLINE='1', HF_HUB_DISABLE_TELEMETRY='1')
    import torch
    import transformers
    from transformers import AutoModel, AutoTokenizer
    root = Path(os.environ['KNOW_THINGS_ASSET_ROOT'])
    bundle = json.loads((root/'bundle.json').read_text())
    manifest = bundle['manifest']
    runtime = f'python={sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro};torch={torch.__version__};transformers={transformers.__version__};threads=2;cpu'
    if runtime != manifest['runtime']:
        raise ValueError('runtime identity mismatch')
    if manifest['pooling'] != 'masked-mean-token-weighted-windows-v2-native-postprocessor' or manifest['normalization'] != 'l2':
        raise ValueError('unsupported pooling or normalization')
    torch.set_num_threads(2)
    tokenizer = AutoTokenizer.from_pretrained(root/'model', local_files_only=True, trust_remote_code=False)
    backend = tokenizer.backend_tokenizer
    backend.no_truncation()
    backend.no_padding()
    model = AutoModel.from_pretrained(root/'model', local_files_only=True, trust_remote_code=False).to('cpu').eval()
    manifest_id = hashlib.sha256(json.dumps(manifest, sort_keys=True, separators=(',', ':'), ensure_ascii=False).encode()).hexdigest()
    send(dict(protocolVersion=1, kind='embedding', manifestId=manifest_id, runtime=runtime))
    while True:
        try:
            request = receive()
        except EOFError:
            return
        request_id = request.get('requestId')
        try:
            if request['protocolVersion'] != 1 or request['operation'] != 'embed':
                raise ValueError('unsupported request')
            text = (manifest['queryPrefix'] if request['query'] else manifest['documentPrefix'])+request['text']
            # Every source token contributes; large blocks are explicitly encoded as
            # non-overlapping windows, rather than silently truncated by the tokenizer.
            encoding = backend.encode(text, add_special_tokens=False)
            tokens = encoding.ids[:]
            window_size = manifest['maxInputTokens']-tokenizer.num_special_tokens_to_add(pair=False)
            if window_size <= 0 or not tokens:
                raise ValueError('invalid input or model limit')
            encoding.truncate(window_size, stride=0)
            windows = [encoding]+encoding.overflowing
            if [token for window in windows for token in window.ids] != tokens:
                raise ValueError('token window coverage mismatch')
            total, count = None, 0
            with torch.inference_mode():
                for window in windows:
                    processed = backend.post_process(window, add_special_tokens=True)
                    if len(processed.ids) > manifest['maxInputTokens']:
                        raise ValueError('token window limit exceeded')
                    encoded = {'input_ids': torch.tensor([processed.ids], dtype=torch.long),
                               'attention_mask': torch.tensor([processed.attention_mask], dtype=torch.long)}
                    if 'token_type_ids' in tokenizer.model_input_names:
                        encoded['token_type_ids'] = torch.tensor([processed.type_ids], dtype=torch.long)
                    output = model(**encoded).last_hidden_state
                    mask = encoded['attention_mask'].unsqueeze(-1).expand(output.size()).float()
                    pooled = (output*mask).sum(1)/mask.sum(1).clamp(min=1e-9)
                    weighted = pooled*len(window.ids)
                    total = weighted if total is None else total+weighted
                    count += len(window.ids)
                vector = torch.nn.functional.normalize(total/count, p=2, dim=1)[0].tolist()
            if len(vector) != manifest['dimensions']:
                raise ValueError('dimension mismatch')
            send(dict(requestId=request_id, status='ok', vector=vector, inputTokens=len(tokens), windows=len(windows)))
        except Exception:
            # Deliberately exclude exception details, which can contain source text.
            send(dict(requestId=request_id, status='error', code='embedding_failed'))

if __name__ == '__main__':
    try:
        main()
    except Exception:
        print('Local embedding worker startup/protocol failure', file=sys.stderr)
        sys.exit(1)
