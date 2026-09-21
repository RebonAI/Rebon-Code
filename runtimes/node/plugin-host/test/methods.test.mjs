import test from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import { VALIDATORS, MAX_NAME_BYTES, validateName, validatePluginId } from '../src/methods.mjs';
const manifestUrl=new URL('../../../../crates/rebon-plugin-protocol/tests/fixtures/v1/methods.json',import.meta.url);

test('shared v1 method-payload corpus: every case agrees with the Rust schemas',async(t)=>{const manifest=JSON.parse(await readFile(manifestUrl,'utf8'));assert.equal(manifest.version,1);assert.ok(manifest.cases.length>=30,'the corpus lost cases');const names=new Set(),kinds=new Set();for(const item of manifest.cases)await t.test(item.name,()=>{assert.ok(!names.has(item.name),`${item.name} appears twice`);names.add(item.name);kinds.add(item.kind);const validate=VALIDATORS[item.kind];assert.ok(validate,`corpus names an unknown payload kind ${item.kind}`);assert.ok(Boolean(item.accept)!==Boolean(item.code),'a case must be exactly one of accepted or refused');if(item.code)assert.throws(()=>validate(item.payload),{code:item.code});else validate(item.payload);});assert.deepEqual([...kinds].sort(),['command_invoke','event_deliver','event_emit','event_subscribe','event_unsubscribe','llm_control','llm_stream','plugin_drain','plugin_load','plugin_ready','plugin_unload','seat_call','service_call','tool_invoke']);});

// Rust bounds a name by UTF-8 bytes; measuring UTF-16 units here would accept
// names the other host rejects.
test('name length is measured in UTF-8 bytes, not UTF-16 units',()=>{const wide='一'.repeat(43);assert.equal(wide.length,43);assert.equal(Buffer.byteLength(wide,'utf8'),129);assert.throws(()=>validateName('service',wide),{code:'[NAME_TOO_LONG]'});const fits='一'.repeat(42);assert.equal(Buffer.byteLength(fits,'utf8'),126);assert.equal(validateName('service',fits),fits);assert.equal(Buffer.byteLength('x'.repeat(MAX_NAME_BYTES),'utf8'),MAX_NAME_BYTES);});

test('control characters are rejected across the whole Cc category',()=>{for(const code of [0x00,0x01,0x1f,0x7f,0x9f])assert.throws(()=>validateName('topic',`a${String.fromCodePoint(code)}b`),{code:'[CONTROL_CHARACTER]'},`U+${code.toString(16)}`);assert.equal(validateName('topic','a b'),'a b');});

test('the platform identity is refused after the generic name rules',()=>{assert.throws(()=>validatePluginId(''),{code:'[EMPTY_NAME]'});assert.throws(()=>validatePluginId('$rebon/platform'),{code:'[RESERVED_PLUGIN_ID]'});assert.equal(validatePluginId('plugin.a'),'plugin.a');});

test('validated payloads are frozen and drop nothing they accepted',()=>{const loaded=VALIDATORS.plugin_load({pluginId:'plugin.a',root:'/pkg',entry:'index.mjs',services:['compose']});assert.ok(Object.isFrozen(loaded));assert.deepEqual(loaded.services,['compose']);assert.deepEqual(loaded.eventTopics,[]);const call=VALIDATORS.service_call({service:'compose',request:{n:1}});assert.deepEqual(call.request,{n:1});});

test('declaration limits are checked before the entries they bound',()=>{const many=Array.from({length:257},(_,index)=>String(index));assert.throws(()=>VALIDATORS.plugin_load({pluginId:'p',root:'/pkg',entry:'i',services:many}),{code:'[TOO_MANY_DECLARATIONS]'});const bad=Array.from({length:257},()=>'');assert.throws(()=>VALIDATORS.plugin_load({pluginId:'p',root:'/pkg',entry:'i',services:bad}),{code:'[TOO_MANY_DECLARATIONS]'},'the count is rejected before any entry is read');assert.throws(()=>VALIDATORS.plugin_load({pluginId:'p',root:'/pkg',entry:'i',services:['','']}),{code:'[EMPTY_NAME]'},'and an invalid entry before the duplicate check');});
