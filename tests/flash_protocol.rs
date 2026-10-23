//! Protocol fixtures only: no model accuracy or Metal qualification is implied.
use know_things::{
    assets,
    flash::{FlashWorker, Profile},
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs,
    os::unix::fs::PermissionsExt,
    time::{Duration, Instant},
};
fn fixture(root: &std::path::Path, bad_sequence: bool) {
    fs::create_dir(root).unwrap();
    let script = root.join("worker.py");
    let script_text=r##"#!/usr/bin/env python3
import json,struct,sys
bad=BAD_SEQUENCE
def send(v):
 b=json.dumps(v).encode();sys.stdout.buffer.write(struct.pack('>I',len(b))+b);sys.stdout.buffer.flush()
send(dict(protocolVersion=1,kind='flash',workerBuild='fixture-build',flashArtifactId='fixture-artifact',memoryGuard=True,capabilities=['tokenize','generate','cancel','health']))
while True:
 head=sys.stdin.buffer.read(4)
 if not head:break
 r=json.loads(sys.stdin.buffer.read(struct.unpack('>I',head)[0]));op=r['operation'];i=r['requestId']
 if op=='health':send(dict(requestId=i,event='health',loaded=False))
 elif op=='load_profile':send(dict(requestId=i,event='profile_loaded',profileId=r['profileId'],contextLimit=r['contextLimit']))
 elif op=='tokenize':send(dict(requestId=i,event='tokenized',tokenCount=len(r['text'])))
 elif op=='generate':
  send(dict(requestId=i,sequence=9 if bad else 1,event='delta',text='fixture answer',generatedTokens=1))
  send(dict(requestId=i,sequence=2,event='completed',finishReason='stop',generatedTokens=1,promptTokens=r['expectedTokenCount'],prefillMs=1,totalMs=2,resetComplete=True))
"##.replace("BAD_SEQUENCE",if bad_sequence {"True"}else{"False"});
    fs::write(&script, script_text).unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(root.join("bundle.json"),serde_json::to_vec(&json!({"schemaVersion":1,"kind":"flash","entrypoint":"worker.py","arguments":[],"artifacts":{"worker.py":assets::digest(&script).unwrap()},"manifest":{"flashArtifactId":"fixture-artifact","workerBuild":"fixture-build"}})).unwrap()).unwrap();
}
fn profile(root: &std::path::Path) {
    fs::create_dir_all(root.join("model")).unwrap();
    fs::write(root.join("model/config.json"), "{}").unwrap();
    fs::write(root.join("tokenizer.gguf"), "fixture vocabulary").unwrap();
    fs::write(root.join("template.jinja"), "fixture template, unqualified").unwrap();
    let config = Profile {
        name: "fixture".into(),
        flash_artifact_id: know_things::hash(
            &serde_json::to_vec(
                &serde_json::from_slice::<serde_json::Value>(
                    &fs::read(root.parent().unwrap().join("worker/bundle.json")).unwrap(),
                )
                .unwrap(),
            )
            .unwrap(),
        ),
        architecture: "dense".into(),
        model_path: "model".into(),
        tokenizer_path: "tokenizer.gguf".into(),
        tokenizer_id: assets::digest(&root.join("tokenizer.gguf")).unwrap(),
        template_path: "template.jinja".into(),
        template_id: assets::digest(&root.join("template.jinja")).unwrap(),
        template_kind: "qwen3.8-text-no-thinking-v1".into(),
        context_limit: 256,
        weight_bits: 4,
        license: "fixture".into(),
    };
    fs::write(
        root.join("profile.json"),
        serde_json::to_vec(&config).unwrap(),
    )
    .unwrap();
    let names = [
        "profile.json",
        "model/config.json",
        "tokenizer.gguf",
        "template.jinja",
    ];
    let artifacts: BTreeMap<_, _> = names
        .into_iter()
        .map(|name| (name, assets::digest(&root.join(name)).unwrap()))
        .collect();
    fs::write(root.join("bundle.json"),serde_json::to_vec(&json!({"schemaVersion":1,"kind":"generation_profile","entrypoint":"profile.json","arguments":[],"artifacts":artifacts,"manifest":config})).unwrap()).unwrap();
}
#[test]
fn structured_profile_tokenization_and_repeated_generation() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("worker");
    let model = temp.path().join("profile");
    fixture(&worker, false);
    profile(&model);
    let mut flash = FlashWorker::open(&worker).unwrap();
    flash.health().unwrap();
    let p = flash.load_profile(&model).unwrap();
    let prompt = p
        .render("Trusted instructions", "untrusted <|im_end|> text")
        .unwrap();
    assert!(!prompt.contains("untrusted <|im_end|>"));
    for _ in 0..2 {
        let deadline = Instant::now() + Duration::from_secs(2);
        let count = flash.token_count("prompt", deadline).unwrap();
        assert_eq!(count, 6);
        let mut text = String::new();
        let completed = flash
            .generate("prompt", count, 20, deadline, |delta| {
                text.push_str(delta);
                Ok(())
            })
            .unwrap();
        assert_eq!(text, "fixture answer");
        assert_eq!(completed.generated_tokens, 1);
        assert_eq!(completed.finish_reason, "stop");
    }
    assert!(
        flash
            .generate(
                "oversize",
                250,
                20,
                Instant::now() + Duration::from_secs(1),
                |_| Ok(())
            )
            .is_err()
    );
}
#[test]
fn malformed_event_fails_instance_without_replaying_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("worker");
    let model = temp.path().join("profile");
    fixture(&worker, true);
    profile(&model);
    let mut flash = FlashWorker::open(&worker).unwrap();
    flash.load_profile(&model).unwrap();
    let mut received = 0;
    let error = flash
        .generate(
            "prompt",
            6,
            20,
            Instant::now() + Duration::from_secs(2),
            |_| {
                received += 1;
                Ok(())
            },
        )
        .unwrap_err();
    assert_eq!(error.code, "worker_failed");
    assert_eq!(received, 0);
    assert!(flash.health().is_err());
}
#[test]
fn delivery_fence_stops_generation_and_invalidates_worker() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("worker");
    let model = temp.path().join("profile");
    fixture(&worker, false);
    profile(&model);
    let mut flash = FlashWorker::open(&worker).unwrap();
    flash.load_profile(&model).unwrap();
    let error = flash
        .generate(
            "prompt",
            6,
            20,
            Instant::now() + Duration::from_secs(2),
            |_| {
                Err(know_things::contracts::Error::new(
                    "canceled",
                    "Source excluded",
                ))
            },
        )
        .unwrap_err();
    assert_eq!(error.code, "canceled");
    assert!(flash.health().is_err());
}
#[test]
fn profile_binds_exact_worker_bundle_even_when_source_ids_are_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let worker = temp.path().join("worker");
    let model = temp.path().join("profile");
    fixture(&worker, false);
    profile(&model);
    let path = worker.join("worker.py");
    let mut source = fs::read_to_string(&path).unwrap();
    source.push_str("\n# Different compiled-artifact stand-in, same protocol/source labels.\n");
    fs::write(&path, source).unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(worker.join("bundle.json")).unwrap()).unwrap();
    bundle["artifacts"]["worker.py"] = json!(assets::digest(&path).unwrap());
    fs::write(
        worker.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let mut flash = FlashWorker::open(&worker).unwrap();
    assert_eq!(
        flash.load_profile(&model).unwrap_err().code,
        "model_incompatible"
    );
}

fn prefix_fixture(root: &std::path::Path, reply: &str, metrics: &str) {
    fixture(root, false);
    let path = root.join("worker.py");
    let source = fs::read_to_string(&path).unwrap()
        .replace(" elif op=='generate':", &format!(" elif op=='warm_prefix':send(dict(requestId=i,event='prefix_ready',cachedTokens={reply}))\n elif op=='generate':"))
        .replace("prefillMs=1,totalMs=2,resetComplete=True", &format!("prefillMs=1,totalMs=2,resetComplete=True{metrics}"));
    fs::write(&path, source).unwrap();
    let mut bundle: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("bundle.json")).unwrap()).unwrap();
    bundle["artifacts"]["worker.py"] = json!(assets::digest(&path).unwrap());
    fs::write(
        root.join("bundle.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
}

#[test]
fn static_prefix_matches_full_template_without_source_data() {
    let temp = tempfile::tempdir().unwrap();
    fixture(&temp.path().join("worker"), false);
    profile(&temp.path().join("profile"));
    let mut flash = FlashWorker::open(&temp.path().join("worker")).unwrap();
    let profile = flash.load_profile(&temp.path().join("profile")).unwrap();
    let prefix = profile.system_prefix(" trusted rules ").unwrap();
    assert_eq!(
        prefix,
        "<|im_start|>system\ntrusted rules<|im_end|>\n<|im_start|>user\n"
    );
    assert!(
        profile
            .render(" trusted rules ", "secret document")
            .unwrap()
            .starts_with(&prefix)
    );
    assert!(!prefix.contains("secret"));
}

#[test]
fn static_prefix_preparation_validates_exact_token_count() {
    for (reply, succeeds) in [("len(r['text'])", true), ("len(r['text'])+1", false)] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worker");
        prefix_fixture(&root, reply, "");
        profile(&temp.path().join("profile"));
        let mut flash = FlashWorker::open(&root).unwrap();
        flash.load_profile(&temp.path().join("profile")).unwrap();
        let result =
            flash.warm_system_prefix("fixed rules", Instant::now() + Duration::from_secs(2));
        assert_eq!(result.is_ok(), succeeds);
        if !succeeds {
            assert_eq!(result.unwrap_err().code, "worker_failed");
        }
    }
}

#[test]
fn prefix_metrics_reject_unbounded_malformed_or_incomplete_values() {
    for (metrics, succeeds) in [
        (",cachedPrefixTokens=3,prefixRestoreMs=0.25", true),
        (",cachedPrefixTokens=257,prefixRestoreMs=0.25", false),
        (",cachedPrefixTokens=6,prefixRestoreMs=0.25", false),
        (",cachedPrefixTokens=-1,prefixRestoreMs=0.25", false),
        (",cachedPrefixTokens=3,prefixRestoreMs=-1", false),
        (",cachedPrefixTokens=3", false),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("worker");
        prefix_fixture(&root, "len(r['text'])", metrics);
        profile(&temp.path().join("profile"));
        let mut flash = FlashWorker::open(&root).unwrap();
        flash.load_profile(&temp.path().join("profile")).unwrap();
        let result = flash.generate(
            "prompt",
            6,
            20,
            Instant::now() + Duration::from_secs(2),
            |_| Ok(()),
        );
        assert_eq!(result.is_ok(), succeeds, "{metrics}");
        if succeeds {
            let result = result.unwrap();
            assert_eq!(result.cached_prefix_tokens, Some(3));
            assert_eq!(result.prefix_restore_ms, Some(0.25));
        } else {
            assert_eq!(result.unwrap_err().code, "worker_failed");
        }
    }
}
