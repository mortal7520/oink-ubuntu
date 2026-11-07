/* PulseAudio's asynchronous native API also works with pipewire-pulse.
 * All context/stream objects stay on their creating worker thread. */
#include <pulse/pulseaudio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <stdio.h>
typedef struct { char name[512], description[512], monitor[512]; uint32_t rate, channels, index, volume; int muted, is_default; } OinkDevice;
typedef void (*OinkPCM)(void*, float*, size_t, uint64_t, int, int);
typedef struct {
 pa_mainloop *loop; pa_context *context; pa_stream *stream;
 OinkPCM callback; void *user; int record, failed, changed, mute_changed, default_changed, watched;
 uint32_t index, last_volume; int last_muted; char selected[512], default_sink[512], default_source[512];
 OinkDevice *devices; size_t capacity, count;
} OinkPulse;
static uint64_t micros(void) { struct timespec t; clock_gettime(CLOCK_MONOTONIC,&t); return (uint64_t)t.tv_sec*1000000+t.tv_nsec/1000; }
static void pause_tick(void) { const struct timespec t={0,1000000}; nanosleep(&t,NULL); }
static int step(OinkPulse *p) { int r=0; if(pa_mainloop_iterate(p->loop,0,&r)<0) return -1; pa_context_state_t s=pa_context_get_state(p->context); return PA_CONTEXT_IS_GOOD(s)?0:-1; }
static int wait_operation(OinkPulse *p,pa_operation *op) {
 if(!op) return -1;
 uint64_t until=micros()+5000000;
 while(pa_operation_get_state(op)==PA_OPERATION_RUNNING) { if(step(p)<0 || micros()>until) {pa_operation_cancel(op);pa_operation_unref(op);return -1;} pause_tick(); }
 int ok=pa_operation_get_state(op)==PA_OPERATION_DONE; pa_operation_unref(op);return ok?0:-1;
}
void oink_pa_close(OinkPulse *p) { if(!p)return; if(p->stream){pa_stream_set_read_callback(p->stream,NULL,NULL);pa_stream_set_write_callback(p->stream,NULL,NULL);pa_stream_disconnect(p->stream);pa_stream_unref(p->stream);} if(p->context){pa_context_disconnect(p->context);pa_context_unref(p->context);}if(p->loop)pa_mainloop_free(p->loop);free(p); }
static void copy(char *to,const char *from) { snprintf(to,512,"%s",from?from:""); }
static void server_info(pa_context*c,const pa_server_info*i,void*u) { (void)c; OinkPulse*p=u;if(i){if(p->watched&&strcmp(p->default_sink,i->default_sink_name?i->default_sink_name:""))p->default_changed=1;copy(p->default_sink,i->default_sink_name);copy(p->default_source,i->default_source_name);} }
static OinkPulse *open_context(char *error,size_t size) {
 OinkPulse*p=calloc(1,sizeof(*p));if(!p)return NULL;p->loop=pa_mainloop_new();if(!p->loop)goto fail;
 p->context=pa_context_new(pa_mainloop_get_api(p->loop),"Oink");if(!p->context)goto fail;
 if(pa_context_connect(p->context,NULL,PA_CONTEXT_NOAUTOSPAWN,NULL)<0)goto fail;
 uint64_t until=micros()+5000000;
 while(pa_context_get_state(p->context)!=PA_CONTEXT_READY){if(step(p)<0||micros()>until)goto fail;pause_tick();}
 if(wait_operation(p,pa_context_get_server_info(p->context,server_info,p))<0)goto fail;
 return p;
 fail: if(error&&size)snprintf(error,size,"PulseAudio connection: %s",p->context?pa_strerror(pa_context_errno(p->context)):"out of memory");oink_pa_close(p);return NULL;
}
static void sink_info(pa_context*c,const pa_sink_info*i,int eol,void*u) { (void)c; OinkPulse*p=u;if(eol||!i)return;if(p->count>=p->capacity){p->failed=1;return;}OinkDevice*d=&p->devices[p->count++];memset(d,0,sizeof(*d));copy(d->name,i->name);copy(d->description,i->description);copy(d->monitor,i->monitor_source_name);d->rate=i->sample_spec.rate;d->channels=i->sample_spec.channels;d->index=i->index;d->volume=pa_cvolume_avg(&i->volume);d->muted=i->mute;d->is_default=!strcmp(p->default_sink,i->name); }
static void source_info(pa_context*c,const pa_source_info*i,int eol,void*u){(void)c;OinkPulse*p=u;if(eol||!i)return;if(p->count>=p->capacity){p->failed=1;return;}OinkDevice*d=&p->devices[p->count++];memset(d,0,sizeof(*d));copy(d->name,i->name);copy(d->description,i->description);const char*bus=pa_proplist_gets(i->proplist,"device.bus");const char*class_name=pa_proplist_gets(i->proplist,"device.class");int hardware=(i->flags&PA_SOURCE_HARDWARE)||(bus&&(!strcmp(bus,"pci")||!strcmp(bus,"usb")||!strcmp(bus,"bluetooth")))||(class_name&&!strcmp(class_name,"sound"));copy(d->monitor,i->monitor_of_sink!=PA_INVALID_INDEX?"monitor":(hardware?"":"virtual"));d->rate=i->sample_spec.rate;d->channels=i->sample_spec.channels;d->index=i->index;d->volume=pa_cvolume_avg(&i->volume);d->muted=i->mute;d->is_default=!strcmp(p->default_source,i->name);}
int oink_pa_sources(OinkDevice*out,size_t cap,char*error,size_t size){OinkPulse*p=open_context(error,size);if(!p)return -1;p->devices=out;p->capacity=cap;int ok=wait_operation(p,pa_context_get_source_info_list(p->context,source_info,p));int count=(int)p->count;if(p->failed)ok=-1;oink_pa_close(p);return ok<0?-1:count;}
int oink_pa_devices(OinkDevice*out,size_t cap,char*error,size_t size) { OinkPulse*p=open_context(error,size);if(!p)return -1;p->devices=out;p->capacity=cap;int ok=wait_operation(p,pa_context_get_sink_info_list(p->context,sink_info,p));int count=(int)p->count;if(p->failed){ok=-1;if(error&&size)snprintf(error,size,"more than %zu PulseAudio sinks",cap);}oink_pa_close(p);return ok<0?-1:count; }
static void control_info(pa_context*c,const pa_sink_info*i,int eol,void*u){(void)c;OinkPulse*p=u;if(eol||!i)return;uint32_t volume=pa_cvolume_avg(&i->volume);if(p->watched&&(p->last_volume!=volume||p->last_muted!=i->mute)){p->changed=1;if(p->last_muted!=i->mute)p->mute_changed=1;}p->last_volume=volume;p->last_muted=i->mute;}
static void subscribed(pa_context*c,pa_subscription_event_type_t t,uint32_t index,void*u) { OinkPulse*p=u;int facility=t&PA_SUBSCRIPTION_EVENT_FACILITY_MASK;if(facility==PA_SUBSCRIPTION_EVENT_SERVER){pa_operation*o=pa_context_get_server_info(c,server_info,p);if(o)pa_operation_unref(o);}else if(facility==PA_SUBSCRIPTION_EVENT_SINK&&index==p->index){if((t&PA_SUBSCRIPTION_EVENT_TYPE_MASK)==PA_SUBSCRIPTION_EVENT_REMOVE){p->failed=1;return;}pa_operation*o=pa_context_get_sink_info_by_index(c,index,control_info,p);if(o)pa_operation_unref(o);} }
static void success(pa_context*c,int ok,void*u){(void)c;*(int*)u=ok;}
int oink_pa_watch(OinkPulse*p){if(wait_operation(p,pa_context_get_sink_info_by_index(p->context,p->index,control_info,p))<0)return -1;pa_context_set_subscribe_callback(p->context,subscribed,p);int ok=0;if(wait_operation(p,pa_context_subscribe(p->context,PA_SUBSCRIPTION_MASK_SINK|PA_SUBSCRIPTION_MASK_SERVER,success,&ok))<0||!ok)return -1;p->watched=1;return 0;}
static void overflow(pa_stream*s,void*u){(void)s;((OinkPulse*)u)->failed=1;}
static void capture_read(pa_stream*s,size_t length,void*u) {
 (void)length;OinkPulse*p=u;const void*data;size_t bytes;
 while(pa_stream_readable_size(s)>0){if(pa_stream_peek(s,&data,&bytes)<0){p->failed=1;return;}if(!bytes)break;
 pa_usec_t latency=0;int negative=0;int known=pa_stream_get_latency(s,&latency,&negative)>=0&&!negative;
 p->callback(p->user,(float*)data,bytes/sizeof(float),latency,known,0);if(pa_stream_drop(s)<0){p->failed=1;return;}}
}
static void playback_write(pa_stream*s,size_t length,void*u) {
 OinkPulse*p=u;while(length>=sizeof(float)*2){size_t bytes=length>8192?8192:length;void*data=NULL;
 if(pa_stream_begin_write(s,&data,&bytes)<0||!data){p->failed=1;return;}bytes-=bytes%(sizeof(float)*2);
 if(!bytes){pa_stream_cancel_write(s);return;}pa_usec_t latency=0;int negative=0;int known=pa_stream_get_latency(s,&latency,&negative)>=0&&!negative;
 p->callback(p->user,data,bytes/sizeof(float),latency,known,1);
 if(pa_stream_write(s,data,bytes,NULL,0,PA_SEEK_RELATIVE)<0){p->failed=1;return;}length-=bytes;
 }
}
OinkPulse *oink_pa_stream(const char*device,int record,uint32_t rate,uint32_t frames,OinkPCM callback,void*user,char*error,size_t size) {
 OinkPulse*p=open_context(error,size);if(!p)return NULL;p->callback=callback;p->user=user;p->record=record;
 OinkDevice devices[64];p->devices=devices;p->capacity=64;
 if(wait_operation(p,record==2?pa_context_get_source_info_list(p->context,source_info,p):pa_context_get_sink_info_list(p->context,sink_info,p))<0||p->failed)goto fail;
 OinkDevice *selected=NULL;for(size_t n=0;n<p->count;n++)if((device&&*device)?!strcmp(device,devices[n].name):devices[n].is_default)selected=&devices[n];
 if(!selected){if(error&&size)snprintf(error,size,"selected PulseAudio sink is unavailable");oink_pa_close(p);return NULL;}
 p->index=selected->index;copy(p->selected,selected->name);p->devices=NULL;
 pa_sample_spec spec={PA_SAMPLE_FLOAT32NE,rate,2};p->stream=pa_stream_new(p->context,record?"Oink output monitor":"Oink received audio",&spec,NULL);if(!p->stream)goto fail;
 pa_buffer_attr attr;memset(&attr,255,sizeof(attr));attr.maxlength=rate*2*sizeof(float)/10;
 if(record){attr.fragsize=(frames?frames:240)*2*sizeof(float);pa_stream_set_read_callback(p->stream,capture_read,p);pa_stream_set_overflow_callback(p->stream,overflow,p);}
 else{attr.tlength=(frames?frames:240)*2*sizeof(float)*2;attr.minreq=(frames?frames:240)*2*sizeof(float);/* Retain automatic prebuffering: prebuf=0 lets read outrun write after underrun. */pa_stream_set_write_callback(p->stream,playback_write,p);}
 pa_stream_flags_t flags=PA_STREAM_AUTO_TIMING_UPDATE|PA_STREAM_INTERPOLATE_TIMING|PA_STREAM_ADJUST_LATENCY|PA_STREAM_DONT_MOVE;
 int rc=record?pa_stream_connect_record(p->stream,record==2?selected->name:selected->monitor,&attr,flags):pa_stream_connect_playback(p->stream,selected->name,&attr,flags,NULL,NULL);
 if(rc<0)goto fail;
 uint64_t until=micros()+5000000;
 while(pa_stream_get_state(p->stream)!=PA_STREAM_READY){if(!PA_STREAM_IS_GOOD(pa_stream_get_state(p->stream))||step(p)<0||micros()>until)goto fail;pause_tick();}
 return p;
 fail:if(error&&size)snprintf(error,size,"PulseAudio stream: %s",pa_strerror(pa_context_errno(p->context)));oink_pa_close(p);return NULL;
}
int oink_pa_iterate(OinkPulse*p){if(step(p)<0||p->failed||(p->stream&&!PA_STREAM_IS_GOOD(pa_stream_get_state(p->stream))))return -1;return p->default_changed?2:(p->changed?1:0);}
/* A playback writer must flush ready/deferred I/O before sleeping again.
 * A bounded native poll also keeps stop/join responsive without busy waiting. */
int oink_pa_playback_iterate(OinkPulse*p){
 if(pa_mainloop_prepare(p->loop,1000)<0||pa_mainloop_poll(p->loop)<0||pa_mainloop_dispatch(p->loop)<0)return -1;
 if(p->failed||!PA_CONTEXT_IS_GOOD(pa_context_get_state(p->context))||(p->stream&&!PA_STREAM_IS_GOOD(pa_stream_get_state(p->stream))))return -1;
 return 0;
}
uint32_t oink_pa_buffer(OinkPulse*p){const pa_buffer_attr*a=pa_stream_get_buffer_attr(p->stream);if(!a)return 0;uint32_t bytes=p->record?a->fragsize:a->minreq;return bytes==(uint32_t)-1?0:bytes/(sizeof(float)*2);}
const char*oink_pa_selected(OinkPulse*p){return p->selected;}
static int set_control(const char*name,int mute,uint32_t volume,int set_volume,char*error,size_t size){OinkPulse*p=open_context(error,size);if(!p)return -1;int ok=0;pa_operation*op;if(set_volume){OinkDevice d;p->devices=&d;p->capacity=1;if(wait_operation(p,pa_context_get_sink_info_by_name(p->context,name,sink_info,p))<0||p->count!=1){oink_pa_close(p);return -1;}pa_cvolume v;pa_cvolume_set(&v,(uint8_t)d.channels,volume);op=pa_context_set_sink_volume_by_name(p->context,name,&v,success,&ok);}else op=pa_context_set_sink_mute_by_name(p->context,name,mute,success,&ok);int rc=wait_operation(p,op);if((rc<0||!ok)&&error&&size)snprintf(error,size,"PulseAudio endpoint control: %s",pa_strerror(pa_context_errno(p->context)));oink_pa_close(p);return rc<0||!ok?-1:0;}
int oink_pa_mute(const char*name,int mute,char*error,size_t size){return set_control(name,mute,0,0,error,size);}
int oink_pa_volume(const char*name,uint32_t volume,char*error,size_t size){return set_control(name,0,volume,1,error,size);}

static void inventory_event(pa_context*c,pa_subscription_event_type_t t,uint32_t index,void*u){(void)c;(void)index;int facility=t&PA_SUBSCRIPTION_EVENT_FACILITY_MASK;OinkPulse*p=u;p->changed|=facility==PA_SUBSCRIPTION_EVENT_SOURCE_OUTPUT?2:(facility==PA_SUBSCRIPTION_EVENT_SINK_INPUT?4:1);}
OinkPulse*oink_pa_inventory_open(char*error,size_t size){OinkPulse*p=open_context(error,size);if(!p)return NULL;pa_context_set_subscribe_callback(p->context,inventory_event,p);int ok=0;if(wait_operation(p,pa_context_subscribe(p->context,PA_SUBSCRIPTION_MASK_SINK|PA_SUBSCRIPTION_MASK_SOURCE|PA_SUBSCRIPTION_MASK_SERVER|PA_SUBSCRIPTION_MASK_SOURCE_OUTPUT|PA_SUBSCRIPTION_MASK_SINK_INPUT,success,&ok))<0||!ok){oink_pa_close(p);return NULL;}return p;}
int oink_pa_take_changed(OinkPulse*p){int result=p->changed;p->changed=0;return result;}
typedef struct{uint32_t source,*ids;size_t capacity,count;int found,overflow;} OinkDemand;
static void demand_source(pa_context*c,const pa_source_info*i,int eol,void*u){(void)c;(void)eol;OinkDemand*d=u;if(i){d->source=i->index;d->found=1;}}
static void demand_stream(pa_context*c,const pa_source_output_info*i,int eol,void*u){(void)c;(void)eol;OinkDemand*d=u;if(i&&i->source==d->source&&!i->corked){if(d->count<d->capacity)d->ids[d->count++]=i->index;else d->overflow=1;}}
int oink_pa_consumers(uint32_t*out,size_t cap,char*error,size_t size){OinkPulse*p=open_context(error,size);if(!p)return -1;OinkDemand demand;memset(&demand,0,sizeof(demand));demand.ids=out;demand.capacity=cap;if(wait_operation(p,pa_context_get_source_info_by_name(p->context,"oink_microphone",demand_source,&demand))<0||!demand.found){if(error&&size)snprintf(error,size,"Oink virtual microphone is unavailable");oink_pa_close(p);return -1;}int ok=wait_operation(p,pa_context_get_source_output_info_list(p->context,demand_stream,&demand));oink_pa_close(p);if(demand.overflow){if(error&&size)snprintf(error,size,"too many virtual microphone consumers");return -1;}return ok<0?-1:(int)demand.count;}

int oink_pa_mute_changed(OinkPulse*p){return p->mute_changed;}

static void watch_output_info(pa_context*c,const pa_sink_info*i,int eol,void*u){(void)c;OinkPulse*p=u;if(eol<0)p->failed=1;if(i){p->index=i->index;p->last_volume=pa_cvolume_avg(&i->volume);p->last_muted=i->mute;}}
int oink_pa_watch_output(OinkPulse*p,const char*name){if(wait_operation(p,pa_context_get_sink_info_by_name(p->context,name,watch_output_info,p))<0||p->failed)return -1;return oink_pa_watch(p);}

typedef struct{uint32_t index;char name[128],argument[2048];} OinkModule;
typedef struct{OinkModule*out;size_t capacity,count;int failed;} OinkModules;
static void module_info(pa_context*c,const pa_module_info*i,int eol,void*u){(void)c;(void)eol;OinkModules*list=u;if(!i)return;if(list->count>=list->capacity||strlen(i->name)>=128||(i->argument&&strlen(i->argument)>=2048)){list->failed=1;return;}OinkModule*m=&list->out[list->count++];memset(m,0,sizeof(*m));m->index=i->index;snprintf(m->name,sizeof(m->name),"%s",i->name);snprintf(m->argument,sizeof(m->argument),"%s",i->argument?i->argument:"");}
int oink_pa_modules(OinkModule*out,size_t cap,char*error,size_t size){OinkPulse*p=open_context(error,size);if(!p)return -1;OinkModules list={out,cap,0,0};int ok=wait_operation(p,pa_context_get_module_info_list(p->context,module_info,&list));oink_pa_close(p);if(list.failed){if(error&&size)snprintf(error,size,"PulseAudio module inventory exceeds safe bounds");return -1;}return ok<0?-1:(int)list.count;}
