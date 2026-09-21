import test from 'node:test';
import assert from 'node:assert/strict';
import path from 'node:path';
import { loadPlugin, resolveEntry } from '../src/loader.mjs';

const request=(over={})=>({pluginId:'plugin.a',root:path.resolve('/packages/demo'),entry:'index.mjs',services:['compose'],eventTopics:['session'],...over});
const module_=(activate)=>async()=>({activate});

test('an entry resolves inside its package root',()=>{const root=path.resolve('/packages/demo');const url=resolveEntry(root,'src/index.mjs');assert.ok(url.startsWith('file:'));assert.ok(decodeURIComponent(url).includes('src'));});

// The payload validator rejects `..` in the text; this is the check that still
// holds when the text looked innocent.
test('an entry that resolves outside the root is refused',()=>{const root=path.resolve('/packages/demo');assert.throws(()=>resolveEntry(root,'../other/index.mjs'),{code:'[PATH_ESCAPES]'});assert.throws(()=>resolveEntry(root,path.resolve('/etc/passwd')),{code:'[PATH_ESCAPES]'});});

// A sibling directory whose name merely starts with the root's is outside it.
test('a root prefix is not the same as being inside the root',()=>{const root=path.resolve('/packages/demo');assert.throws(()=>resolveEntry(root,'../demo-evil/index.mjs'),{code:'[PATH_ESCAPES]'});});

test('activation collects what the plugin registered',async()=>{const loaded=await loadPlugin(request(),module_((plugin)=>{plugin.service('compose',async(x)=>x);plugin.topic('session',()=>{});}));assert.deepEqual([...loaded.services],['compose']);assert.deepEqual([...loaded.eventTopics],['session']);assert.equal(typeof loaded.serviceHandlers.get('compose'),'function');});

// The manifest is the ceiling, and the refusal names the plugin's own line
// rather than arriving later as a rejected ready report.
test('registering something the manifest did not declare is refused',async()=>{await assert.rejects(loadPlugin(request(),module_((plugin)=>plugin.service('smuggled',()=>{}))),{code:'[UNAUTHORIZED_REGISTER]'});await assert.rejects(loadPlugin(request(),module_((plugin)=>plugin.topic('smuggled',()=>{}))),{code:'[UNAUTHORIZED_REGISTER]'});});

test('registering less than was declared is allowed',async()=>{const loaded=await loadPlugin(request(),module_(()=>{}));assert.deepEqual([...loaded.services],[]);});

test('a service registered twice is refused',async()=>{await assert.rejects(loadPlugin(request(),module_((plugin)=>{plugin.service('compose',()=>{});plugin.service('compose',()=>{});})),{code:'[DUPLICATE_DECLARATION]'});});

test('a handler must be a function',async()=>{await assert.rejects(loadPlugin(request(),module_((plugin)=>plugin.service('compose','nope'))),{code:'[WRONG_SHAPE]'});});

// Registration closes when activate returns: anything later would add
// capabilities the host already reported as the complete set.
test('registering after activation is refused',async()=>{let escaped;await loadPlugin(request(),module_((plugin)=>{escaped=plugin;}));assert.throws(()=>escaped.service('compose',()=>{}),{code:'[REGISTRATION_CLOSED]'});});

test('a module without activate is refused',async()=>{await assert.rejects(loadPlugin(request(),async()=>({})),{code:'[NO_ACTIVATE]'});await assert.rejects(loadPlugin(request(),async()=>({activate:42})),{code:'[NO_ACTIVATE]'});});

test('a default export counts as activate',async()=>{const loaded=await loadPlugin(request(),async()=>({default:(plugin)=>plugin.service('compose',()=>{})}));assert.deepEqual([...loaded.services],['compose']);});

test('an import failure and an activation failure are told apart',async()=>{await assert.rejects(loadPlugin(request(),async()=>{throw new Error('no such file');}),{code:'[ENTRY_FAILED]'});await assert.rejects(loadPlugin(request(),module_(()=>{throw new Error('boom');})),{code:'[ACTIVATE_FAILED]'});});

test('an async activate is awaited before registration closes',async()=>{const loaded=await loadPlugin(request(),module_(async(plugin)=>{await new Promise((r)=>setImmediate(r));plugin.service('compose',()=>{});}));assert.deepEqual([...loaded.services],['compose']);});

// The package says what a plugin can do; the load request's `config` says what
// this installation wants it to do. The loader carries it and reads none of it.
test('the load request config reaches activate verbatim',async()=>{let seen='unset';const config={baseURL:'https://example.invalid',models:[{id:'m'}]};await loadPlugin(request({config}),module_((_plugin,value)=>{seen=value;}));assert.deepEqual(seen,config);});

test('a load with no config hands activate whatever the request carried',async()=>{let seen='unset';await loadPlugin(request(),module_((_plugin,value)=>{seen=value;}));assert.equal(seen,undefined);});
