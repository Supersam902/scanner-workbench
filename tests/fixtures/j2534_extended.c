/* Test-only native ABI fixture. No USB, vehicle access or vendor code. */
#include <stdint.h>
#include <stdatomic.h>
#include <string.h>
#ifdef _WIN32
#define API __declspec(dllexport) __stdcall
#else
#define API
#endif
typedef struct { uint32_t protocol,rx_status,tx_flags,timestamp,data_size,extra_data_index; uint8_t data[4128]; } Message;
typedef struct {uint32_t count;uint8_t *bytes;} ByteArray;
static uint32_t protocol, rate, request_id, stage, filters, stopped, initialized, periodic, periodic_stops, closes;
static _Atomic uint32_t scenario, opens, channel_opens, voltage_reads;
static uint8_t request[128];static uint32_t request_len;
uint32_t API MockCounter(uint32_t counter) {switch(counter){case 0:return filters;case 1:return stopped;case 2:return initialized;case 3:return periodic;case 4:return periodic_stops;case 6:return opens;case 7:return channel_opens;case 8:return voltage_reads;default:return closes;}}
void API MockScenario(uint32_t value){scenario=value;}
uint32_t API PassThruOpen(const void *name,uint32_t *device){(void)name;if(scenario==4)return 8;opens++;*device=11;filters=0;initialized=0;return 0;}
uint32_t API PassThruClose(uint32_t device){if(device!=11)return 3;closes++;return 0;}
uint32_t API PassThruConnect(uint32_t device,uint32_t p,uint32_t flags,uint32_t baud,uint32_t *channel){(void)flags;if(device!=11||!((p==6&&(baud==250000||baud==500000))||((p==3||p==4)&&baud==10400)))return 3;channel_opens++;protocol=p;rate=baud;*channel=22;return 0;}
uint32_t API PassThruDisconnect(uint32_t channel){return channel==22?0:3;}
uint32_t API PassThruStartMsgFilter(uint32_t channel,uint32_t type,Message *mask,Message *pattern,Message *flow,uint32_t *id){if(channel!=22||mask->protocol!=protocol||pattern->protocol!=protocol)return 3;if(protocol==6&&(type!=3||!flow))return 3;if(protocol!=6&&(type!=1||flow))return 3;*id=++filters;return 0;}
uint32_t API PassThruStopMsgFilter(uint32_t channel,uint32_t id){(void)id;if(channel!=22)return 3;filters--;stopped++;return 0;}
uint32_t API PassThruStartPeriodicMsg(uint32_t channel,Message *message,uint32_t *id,uint32_t interval){if(channel!=22||message->protocol!=protocol||interval!=2000||!initialized)return 3;*id=++periodic;return 0;}
uint32_t API PassThruStopPeriodicMsg(uint32_t channel,uint32_t id){(void)id;if(channel!=22)return 3;periodic_stops++;return 0;}
uint32_t API PassThruIoctl(uint32_t handle,uint32_t command,void *input,void *output){
 if(scenario==4)return 8;
 if(command==3&&handle==11){uint32_t sample=voltage_reads++;*(uint32_t*)output=12600+(scenario==3?100*(sample%8):0);return 0;}
 if(handle!=22)return 3;
 if(command==8){stage=99;return 0;}
 if(command==4){ByteArray *in=(ByteArray*)input,*out=(ByteArray*)output;if(in->count!=1||in->bytes[0]!=0x33||out->count<2)return 3;out->bytes[0]=protocol==3?0x08:0x8f;out->bytes[1]=protocol==3?0x08:0xe9;out->count=2;initialized++;return 0;}
 if(command==5){Message *in=(Message*)input,*out=(Message*)output;if(in->protocol!=4||in->data_size!=4||in->data[3]!=0x81)return 3;memset(out,0,sizeof(*out));out->protocol=4;out->data_size=6;out->extra_data_index=6;uint8_t response[]={0x83,0xf1,0x10,0xc1,0x8f,0xe9};memcpy(out->data,response,6);initialized++;return 0;}
 return 1;
}
uint32_t API PassThruWriteMsgs(uint32_t channel,Message *message,uint32_t *count,uint32_t timeout){(void)timeout;if(channel!=22||*count!=1||message->protocol!=protocol)return 3;if(scenario==4)return 8;if(scenario==3&&rate==500000){*count=0;return 9;}uint32_t start=protocol==6?4:3;if(protocol!=6&&!initialized)return 3;if(message->data_size<start||message->data_size-start>128)return 3;request_len=message->data_size-start;memcpy(request,message->data+start,request_len);request_id=protocol==6?((uint32_t)message->data[0]<<24)|((uint32_t)message->data[1]<<16)|((uint32_t)message->data[2]<<8)|message->data[3]:0x10;stage=0;return 0;}
uint32_t API PassThruReadMsgs(uint32_t channel,Message *message,uint32_t *count,uint32_t timeout){
 (void)timeout;if(channel!=22)return 3;if(scenario==4)return 8;if(stage++){*count=0;return 0x10;}
 memset(message,0,sizeof(*message));message->protocol=protocol;
 uint32_t start=protocol==6?4:3;
 if(protocol==6){uint32_t id=request_id==0x7df?0x7e8:request_id+8;message->data[0]=id>>24;message->data[1]=id>>16;message->data[2]=id>>8;message->data[3]=id;}
 else{message->data[0]=protocol==3?0x48:0x86;message->data[1]=0xf1;message->data[2]=0x10;}
 uint8_t *data=message->data+start;uint32_t len=0;
 if(scenario==1){uint8_t negative[]={0x7f,request[0],0x22};memcpy(data,negative,3);len=3;}
 else if(request[0]==1){data[0]=0x41;data[1]=request[1];data[2]=0xbe;data[3]=0x3f;data[4]=0xa8;data[5]=0x13;len=6;}
 else if(request[0]==0x21){data[0]=0x61;data[1]=request[1];data[2]=0x40;data[3]=0;data[4]=0x45;data[5]=0;len=6;}
 else if(request[0]==0x22){data[0]=0x62;data[1]=request[1];data[2]=request[2];data[3]=0;data[4]=80;len=5;}
 else if(request[0]==0x19){uint8_t codes[]={0x59,request[1],0xff,0x12,0x34,0x56,9};memcpy(data,codes,7);len=7;}
 else {data[0]=request[0]+0x40;len=1;}
 message->data_size=start+len;message->extra_data_index=start+len;
 if(protocol!=6){message->data[message->data_size++]=0xaa;} /* checksum must not become data */
 *count=1;return 0;
}
uint32_t API PassThruGetLastError(char *buffer){strcpy(buffer,"extended test fixture");return 0;}
