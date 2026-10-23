// Know-Things framed Flash worker. Compile only against a captured artifact.
#import <Foundation/Foundation.h>
#include "dense.h"
#include "model.h"
#include "llama.h"
#include "reasoning_filter.h"
#include <atomic>
#include <algorithm>
#include <signal.h>
#include <dispatch/dispatch.h>
#include <chrono>
#include <cerrno>
#include <fcntl.h>
#include <functional>
#include <memory>
#include <mutex>
#include <poll.h>
#include <stdexcept>
#include <string>
#include <thread>
#include <unistd.h>
#include <vector>

#ifndef KNOW_THINGS_FLASH_ARTIFACT_ID
#error "A pinned Flash artifact identity is required"
#endif
#ifndef KNOW_THINGS_WORKER_BUILD
#error "A worker build identity is required"
#endif
namespace {
constexpr uint32_t MAX_FRAME=1024*1024;
std::atomic<uint64_t> active{0}, canceled_request{0};
std::atomic<bool> canceled{false}, shutdown_requested{false}, pressure{false};
using Clock=std::chrono::steady_clock;
bool read_exact(int fd, void *dst, size_t n) {
    auto *bytes=(char*)dst;
    while(n) { auto k=::read(fd,bytes,n); if(k==0)return false; if(k<0){if(errno==EINTR)continue;throw std::runtime_error("read_failed");}bytes+=k;n-=k; }
    return true;
}
NSDictionary *receive(int fd) {
    unsigned char head[4];if(!read_exact(fd,head,4))return nil;
    uint32_t n=((uint32_t)head[0]<<24)|((uint32_t)head[1]<<16)|((uint32_t)head[2]<<8)|head[3];
    if(!n||n>MAX_FRAME)throw std::runtime_error("invalid_frame");
    NSMutableData *data=[NSMutableData dataWithLength:n];if(!read_exact(fd,data.mutableBytes,n))throw std::runtime_error("incomplete_frame");
    id value=[NSJSONSerialization JSONObjectWithData:data options:0 error:nil];
    if(![value isKindOfClass:[NSDictionary class]])throw std::runtime_error("invalid_json");return value;
}
void write_all(const void *source,size_t n,Clock::time_point deadline) {
    auto *bytes=(const char*)source;
    while(n) {
        if(Clock::now()>=deadline)throw std::runtime_error("slow_consumer");
        pollfd p{STDOUT_FILENO,POLLOUT,0};if(::poll(&p,1,100)<=0)continue;
        auto k=::write(STDOUT_FILENO,bytes,n);
        if(k<0){if(errno==EAGAIN||errno==EINTR)continue;throw std::runtime_error("event_pipe_failed");}
        if(k==0)throw std::runtime_error("event_pipe_failed");bytes+=k;n-=k;
    }
}
void send(NSDictionary *event) {
    NSData *data=[NSJSONSerialization dataWithJSONObject:event options:0 error:nil];
    if(!data||data.length>MAX_FRAME)throw std::runtime_error("invalid_event");
    uint32_t n=(uint32_t)data.length;unsigned char head[4]={(unsigned char)(n>>24),(unsigned char)(n>>16),(unsigned char)(n>>8),(unsigned char)n};
    auto deadline=Clock::now()+std::chrono::seconds(5);write_all(head,4,deadline);write_all(data.bytes,n,deadline);
}
std::string string(NSDictionary *object,NSString *key) {
    id value=object[key];if(![value isKindOfClass:[NSString class]])throw std::runtime_error("invalid_string");
    return std::string([value UTF8String],[value lengthOfBytesUsingEncoding:NSUTF8StringEncoding]);
}
int number(NSDictionary *object,NSString *key,int low,int high) {
    id value=object[key];if(![value isKindOfClass:[NSNumber class]])throw std::runtime_error("invalid_number");
    double d=[value doubleValue];int n=[value intValue];if(d!=n||n<low||n>high)throw std::runtime_error("invalid_number");return n;
}
std::vector<int32_t> tokenize(const llama_vocab *vocab,const std::string &text) {
    std::vector<int32_t> tokens(text.size()+32);
    int n=llama_tokenize(vocab,text.data(),(int)text.size(),tokens.data(),(int)tokens.size(),true,true);
    if(n<0)throw std::runtime_error("tokenization_failed");tokens.resize(n);return tokens;
}
void guard(Clock::time_point deadline) {
    if(canceled||shutdown_requested||(active.load()!=0&&canceled_request.load()==active.load()))throw std::runtime_error("canceled");
    if(pressure)throw std::runtime_error("resource_exhausted");
    if(Clock::now()>=deadline)throw std::runtime_error("deadline_exceeded");
}
struct Engine {
    std::unique_ptr<flash::Metal> metal;
    std::unique_ptr<flash::DenseModel> dense;
    std::unique_ptr<flash::Model> moe;
    llama_model *tokenizer=nullptr;
    const llama_vocab *vocab=nullptr;
    int context=0;
    std::string profile;
    std::unique_ptr<flash::DenseModel::Snapshot> prefix;
    std::vector<int32_t> prefix_tokens;
    ~Engine(){if(tokenizer)llama_model_free(tokenizer);}
    void reset(){if(dense)dense->reset();if(moe){moe->sync();moe->reset();}}
    int ubatch()const{return dense?dense->ubatch():moe->ubatch();}
    void forward(const int32_t *tokens,int n){if(dense)dense->forward(tokens,n,true,true);else moe->forward(tokens,n,true,true);}
    int32_t top(){return dense?dense->top1(0).first:moe->top1(0).first;}
    size_t kv(){return dense?dense->kv_bytes():moe->kv_bytes();}
};
void warm_prefix(Engine &engine,NSDictionary *request,uint64_t id) {
    if(!engine.dense)throw std::runtime_error("unsupported_prefix_cache");
    if(engine.prefix)throw std::runtime_error("prefix_already_prepared");
    auto text=string(request,@"text");
    const std::string start="<|im_start|>system\n",end="<|im_end|>\n<|im_start|>user\n";
    if(!text.starts_with(start)||!text.ends_with(end)||text.find("<|im_",start.size())!=text.size()-end.size())throw std::runtime_error("invalid_system_prefix");
    auto tokens=tokenize(engine.vocab,text);
    if(tokens.empty()||tokens.size()>256||tokens.size()>=(size_t)engine.context)throw std::runtime_error("prefix_budget_exceeded");
    auto started=Clock::now(),deadline=started+std::chrono::milliseconds(number(request,@"deadlineMs",1,120000));
    engine.reset();
    try {
        for(size_t offset=0;offset<tokens.size();) {
            guard(deadline);int count=(int)std::min(tokens.size()-offset,(size_t)engine.ubatch());
            engine.forward(tokens.data()+offset,count);offset+=count;
        }
        guard(deadline);
        engine.prefix=std::make_unique<flash::DenseModel::Snapshot>(engine.dense->snapshot());
        engine.prefix_tokens=std::move(tokens);engine.reset();
        send(@{@"requestId":@(id),@"event":@"prefix_ready",@"cachedTokens":@(engine.prefix_tokens.size()),@"warmMs":@(std::chrono::duration<double,std::milli>(Clock::now()-started).count())});
    } catch(...) {engine.prefix.reset();engine.prefix_tokens.clear();engine.reset();throw;}
}
void load(Engine &engine,NSDictionary *request,const std::string &root) {
    if(engine.metal)throw std::runtime_error("profile_already_loaded");
    auto kind=string(request,@"architecture"),model=string(request,@"modelPath"),vocab=string(request,@"tokenizerPath");
    if(kind!="dense"&&kind!="moe")throw std::runtime_error("unsupported_architecture");
    int context=number(request,@"contextLimit",256,32768),bits=number(request,@"weightBits",0,8);
    if(bits!=0&&bits!=4&&bits!=8)throw std::runtime_error("unsupported_quantization");
    engine.profile=string(request,@"profileId");engine.context=context;
    auto params=llama_model_default_params();params.vocab_only=true;
    if(engine.tokenizer){llama_model_free(engine.tokenizer);engine.tokenizer=nullptr;engine.vocab=nullptr;}
    engine.tokenizer=llama_model_load_from_file(vocab.c_str(),params);
    if(!engine.tokenizer)throw std::runtime_error("tokenizer_unavailable");engine.vocab=llama_model_get_vocab(engine.tokenizer);
    if(tokenize(engine.vocab,"<think>").size()!=1||tokenize(engine.vocab,"</think>").size()!=1)throw std::runtime_error("unsupported_reasoning_markers");
    engine.metal=std::make_unique<flash::Metal>();engine.metal->load_kernels(root+"/kernels");
    if(kind=="dense")engine.dense=std::make_unique<flash::DenseModel>(*engine.metal,model,context,"",bits);
    else {if(bits!=0)throw std::runtime_error("unsupported_quantization");engine.moe=std::make_unique<flash::Model>(*engine.metal,model,context,16,false);}
}
void generate(Engine &engine,NSDictionary *request,uint64_t id) {
    if(!engine.vocab||(!engine.dense&&!engine.moe))throw std::runtime_error("model_unavailable");
    int maximum=number(request,@"completionLimit",1,4096),expected=number(request,@"expectedTokenCount",1,32768);
    int timeout=number(request,@"deadlineMs",1,120000);
    auto prompt=string(request,@"prompt");auto tokens=tokenize(engine.vocab,prompt);
    if((int)tokens.size()!=expected)throw std::runtime_error("tokenizer_mismatch");
    if(tokens.size()+maximum>(size_t)engine.context)throw std::runtime_error("context_budget_exceeded");
    auto start=Clock::now(),deadline=start+std::chrono::milliseconds(timeout);uint64_t sequence=0;int generated=0;ReasoningFilter visibility;
    active=id;canceled=false;engine.reset();
    size_t cached_tokens=0;double restore_ms=0;
    auto terminal=[&](NSString *status,NSString *reason,double prefill){
        engine.reset();active=0;
        send(@{@"requestId":@(id),@"sequence":@(++sequence),@"event":status,@"finishReason":reason,@"promptTokens":@(tokens.size()),@"generatedTokens":@(generated),@"visibleTokens":@(visibility.visible_tokens),@"hiddenTokens":@(visibility.hidden_tokens),@"prefillMs":@(prefill),@"totalMs":@(std::chrono::duration<double,std::milli>(Clock::now()-start).count()),@"resetComplete":@YES,@"kvBytes":@(engine.kv()),@"cachedPrefixTokens":@(cached_tokens),@"prefixRestoreMs":@(restore_ms)});
    };
    double prefill=0;
    try {
        if(engine.prefix&&engine.prefix_tokens.size()<tokens.size()&&std::equal(engine.prefix_tokens.begin(),engine.prefix_tokens.end(),tokens.begin())) {
            guard(deadline);auto began=Clock::now();engine.dense->restore(*engine.prefix);
            restore_ms=std::chrono::duration<double,std::milli>(Clock::now()-began).count();cached_tokens=engine.prefix_tokens.size();
        }
        for(size_t offset=cached_tokens;offset<tokens.size();) {
            guard(deadline);int count=(int)std::min(tokens.size()-offset,(size_t)engine.ubatch());
            engine.forward(tokens.data()+offset,count);offset+=count;
        }
        prefill=std::chrono::duration<double,std::milli>(Clock::now()-start).count();
        std::string pending;NSString *reason=@"output_limit";
        for(int i=0;i<maximum;i++) {
            guard(deadline);int32_t token=engine.top();
            if(llama_vocab_is_eog(engine.vocab,token)){reason=@"stop";break;}
            generated++;
            std::vector<char> bytes(512);int n=llama_token_to_piece(engine.vocab,token,bytes.data(),(int)bytes.size(),0,true);
            if(n<0){bytes.resize(-n);n=llama_token_to_piece(engine.vocab,token,bytes.data(),(int)bytes.size(),0,true);}
            if(n<0)throw std::runtime_error("token_decode_failed");
            std::string piece(bytes.data(),n);
            auto kind=visibility.accept(piece);
            if(kind==ReasoningFilter::Kind::delimiter) {
                if(!pending.empty())throw std::runtime_error("invalid_utf8_reasoning_boundary");
            }else if(kind==ReasoningFilter::Kind::visible) {
                pending+=piece;
                NSString *text=[[NSString alloc]initWithBytes:pending.data() length:pending.size() encoding:NSUTF8StringEncoding];
                if(text){guard(deadline);send(@{@"requestId":@(id),@"sequence":@(++sequence),@"event":@"delta",@"text":text,@"generatedTokens":@(generated)});pending.clear();}
                if(pending.size()>4096)throw std::runtime_error("invalid_utf8_output");
            }
            if(i+1<maximum)engine.forward(&token,1);
        }
        if(!pending.empty())throw std::runtime_error("incomplete_utf8_output");
        terminal(@"completed",reason,prefill);
    } catch(const std::exception &error) {
        // Error codes are fixed strings above; never propagate source/prompt content.
        terminal(@"failed",[NSString stringWithUTF8String:error.what()],prefill);
    }
}
}
int main() {
    @autoreleasepool {
        try {
            signal(SIGPIPE,SIG_IGN);
            fcntl(STDOUT_FILENO,F_SETFL,fcntl(STDOUT_FILENO,F_GETFL)|O_NONBLOCK);
            const char *asset_root=std::getenv("KNOW_THINGS_ASSET_ROOT"),*control_fd=std::getenv("KNOW_THINGS_CONTROL_FD");
            if(!asset_root||!control_fd)throw std::runtime_error("missing_worker_channels");
            int fd=std::stoi(control_fd);
            std::thread([fd]{@autoreleasepool {try {while(NSDictionary *message=receive(fd)) {
                auto operation=string(message,@"operation");
                if(operation=="cancel") {auto id=[message[@"requestId"] unsignedLongLongValue];if(active.load()==0||active.load()==id)canceled_request=id;}
                else if(operation=="shutdown"){shutdown_requested=true;canceled=true;if(active.load()==0)std::_Exit(0);return;}
                else throw std::runtime_error("unsupported_control");
            }
            shutdown_requested=true;canceled=true;
            if(active.load()==0)std::_Exit(0);
            }catch(...){shutdown_requested=true;canceled=true;if(active.load()==0)std::_Exit(2);}}}).detach();
            dispatch_source_t memory_guard=dispatch_source_create(DISPATCH_SOURCE_TYPE_MEMORYPRESSURE,0,DISPATCH_MEMORYPRESSURE_WARN|DISPATCH_MEMORYPRESSURE_CRITICAL,dispatch_get_global_queue(QOS_CLASS_UTILITY,0));
            dispatch_source_set_event_handler(memory_guard,^{pressure=true;canceled=true;std::fprintf(stderr,"Flash worker stopped for memory pressure\n");std::_Exit(75); });dispatch_resume(memory_guard);
            llama_backend_init();
            Engine engine;
            send(@{@"protocolVersion":@1,@"kind":@"flash",@"workerBuild":@KNOW_THINGS_WORKER_BUILD,@"flashArtifactId":@KNOW_THINGS_FLASH_ARTIFACT_ID,@"capabilities":@[@"dense",@"moe",@"tokenize",@"generate",@"cancel",@"health",@"visible_token_metrics_v1",@"static_system_prefix_v1"],@"loadedProfile":[NSNull null],@"memoryGuard":@YES});
            while(!shutdown_requested) {
                NSDictionary *request=receive(STDIN_FILENO);if(!request)break;
                uint64_t id=[request[@"requestId"] unsignedLongLongValue];
                if(!id||[request[@"protocolVersion"] intValue]!=1)throw std::runtime_error("invalid_request");
                auto operation=string(request,@"operation");
                try {
                    if(operation=="health")send(@{@"requestId":@(id),@"event":@"health",@"loaded":@(engine.dense!=nullptr||engine.moe!=nullptr),@"tokenizerLoaded":@(engine.vocab!=nullptr),@"memoryPressure":@(pressure.load()),@"metalDeviceAvailable":@(MTLCreateSystemDefaultDevice()!=nil)});
                    else if(operation=="load_profile") {guard(Clock::now()+std::chrono::seconds(120));load(engine,request,asset_root);send(@{@"requestId":@(id),@"event":@"profile_loaded",@"contextLimit":@(engine.context),@"profileId":[NSString stringWithUTF8String:engine.profile.c_str()],@"kvBytes":@(engine.kv())});}
                    else if(operation=="load_tokenizer") {
                        if(engine.metal||engine.tokenizer)throw std::runtime_error("tokenizer_already_loaded");
                        auto params=llama_model_default_params();params.vocab_only=true;auto path=string(request,@"tokenizerPath");
                        engine.tokenizer=llama_model_load_from_file(path.c_str(),params);
                        if(!engine.tokenizer)throw std::runtime_error("tokenizer_unavailable");engine.vocab=llama_model_get_vocab(engine.tokenizer);
                        send(@{@"requestId":@(id),@"event":@"tokenizer_loaded"});
                    }
                    else if(operation=="tokenize") {
                        if(!engine.vocab)throw std::runtime_error("model_unavailable");auto tokens=tokenize(engine.vocab,string(request,@"text"));
                        NSMutableDictionary *event=[@{@"requestId":@(id),@"event":@"tokenized",@"tokenCount":@(tokens.size())} mutableCopy];
                        if([request[@"returnTokens"] boolValue]) {
                            if(tokens.size()>65536)throw std::runtime_error("token_limit");NSMutableArray *ids=[NSMutableArray arrayWithCapacity:tokens.size()];
                            for(auto token:tokens)[ids addObject:@(token)];event[@"tokenIds"]=ids;
                        }
                        send(event);
                    }
                    else if(operation=="warm_prefix")warm_prefix(engine,request,id);
                    else if(operation=="generate")generate(engine,request,id);
                    else if(operation=="shutdown")break;
                    else throw std::runtime_error("unsupported_operation");
                }catch(const std::exception &error){
                    send(@{@"requestId":@(id),@"event":@"failed",@"finishReason":@"worker_operation_failed",@"resetComplete":@NO});
                    // A partial load/reset failure makes the instance unusable.
                    return 2;
                }
            }
            dispatch_source_cancel(memory_guard);return 0;
        }catch(...){std::fprintf(stderr,"Flash worker startup/protocol failure\n");return 1;}
    }
}
