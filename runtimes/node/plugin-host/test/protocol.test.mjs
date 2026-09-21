import test from 'node:test';
import assert from 'node:assert/strict';
import { validateEnvelope, envelope, terminal, cancel, request, PLATFORM_PLUGIN_ID, PLATFORM_CONTROL_SCOPE_ID } from '../src/protocol.mjs';
import { encodeFrame } from '../src/framing.mjs';
const base = { protocol_version:1, host_epoch:7, plugin_id:'p', scope_id:'s', scope_generation:3, call_id:'c', message:{type:'request',method:'m',payload:null} };
test('accepts exact three variants and arbitrary JSON payloads', () => {
  for (const message of [{type:'request',method:'',payload:42},{type:'notification',method:'n',payload:null},{type:'terminal',status:'error',payload:[1]}]) assert.equal(validateEnvelope({...base,message}).message.type,message.type);
});
test('rejects non-object and unknown envelope/message keys', () => {
  for (const value of [null,1,[],{...base,x:1},{...base,message:{...base.message,x:1}}]) assert.throws(() => validateEnvelope(value));
});
test('rejects bad version, status and identity field types', () => {
  assert.throws(() => validateEnvelope({...base,protocol_version:2}), {code:'bad_version'});
  assert.throws(() => validateEnvelope({...base,message:{type:'terminal',status:'done',payload:null}}), {code:'bad_status'});
  assert.throws(() => validateEnvelope({...base,plugin_id:1}), {code:'wrong_shape'});
});
test('safe integer bounds and negative zero normalization', () => {
  const value=validateEnvelope({...base,host_epoch:-0,scope_generation:-0}); assert.equal(Object.is(value.host_epoch,-0),false);
  for (const number of [-1,1.5,Number.MAX_SAFE_INTEGER+1]) assert.throws(() => validateEnvelope({...base,host_epoch:number}), {code:'unsafe_integer'});
});
test('empty string identity fields remain valid', () => assert.equal(validateEnvelope({...base,plugin_id:'',scope_id:'',call_id:''}).call_id,''));
test('normalized envelope, message and payload are fresh and frozen', () => {
  const input={...base,message:{...base.message,payload:{nested:[1]}}}; const value=validateEnvelope(input); input.message.payload.nested[0]=2;
  assert.equal(value.message.payload.nested[0],1); assert.ok(Object.isFrozen(value)&&Object.isFrozen(value.message)&&Object.isFrozen(value.message.payload.nested));
});
test('builders revalidate outbound envelopes', () => assert.throws(() => envelope({...base,host_epoch:Infinity}, base.message)));
test('control and cancel constants match exact Rust wire identity', () => {
  assert.equal(PLATFORM_PLUGIN_ID,'$rebon/platform'); assert.equal(PLATFORM_CONTROL_SCOPE_ID,'$rebon/control');
  const control=request({host_epoch:7,plugin_id:PLATFORM_PLUGIN_ID,scope_id:PLATFORM_CONTROL_SCOPE_ID,scope_generation:0,call_id:'control-1'},'platform/initialize',null);
  assert.equal(encodeFrame(control).toString(),'{"protocol_version":1,"host_epoch":7,"plugin_id":"$rebon/platform","scope_id":"$rebon/control","scope_generation":0,"call_id":"control-1","message":{"type":"request","method":"platform/initialize","payload":null}}\n');
  const value=cancel({host_epoch:7,plugin_id:'p',scope_id:'s',scope_generation:3,call_id:'target'}); assert.deepEqual(value.message,{type:'notification',method:'call/cancel',payload:null});
  assert.equal(terminal(value,'success',null).call_id,'target');
});
