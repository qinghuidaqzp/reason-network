// Run with Node.js: node tests/desktop_frontend.cjs. No packages or browser required.
'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const html = fs.readFileSync(path.join(__dirname, '../src/desktop/index.html'), 'utf8');
const script = html.match(/<script nonce="reason-desktop">([\s\S]*?)<\/script>/)[1];
const htmlWrites = [], messages = [], timers = [];
let document;
class Element {
  constructor(tag = 'div') {
    this.tagName = tag; this.children = []; this.dataset = {}; this.style = {};
    this.listeners = {}; this.attributes = {}; this.value = ''; this.hidden = false;
    this.classList = {remove() {}}; this._text = '';
  }
  append(...nodes) { this.children.push(...nodes); }
  replaceChildren(...nodes) { this._text = ''; this.children = nodes; }
  set textContent(value) { this._text = String(value); this.children = []; }
  get textContent() { return this._text + this.children.map(child => child.textContent).join(''); }
  set innerHTML(value) { htmlWrites.push(value); this.children = []; this._text = ''; }
  setAttribute(name, value) { this.attributes[name] = String(value); }
  removeAttribute(name) { delete this.attributes[name]; }
  addEventListener(name, callback) { (this.listeners[name] ??= []).push(callback); }
  emit(name, event = {}) { for (const callback of this.listeners[name] ?? []) callback(event); }
  focus() { document.activeElement = this; }
}
const ids = new Map([...html.matchAll(/\bid="([^"]+)"/g)].map(match => [match[1], new Element()]));
const filters = ['all', 'working', 'unread'].map(filter => Object.assign(new Element('button'), {dataset:{filter}}));
const views = ['overview', 'sessions', 'machines'].map(view => Object.assign(new Element('button'), {dataset:{view}}));
const walk = element => [element, ...element.children.flatMap(walk)];
document = {
  body:new Element('body'), activeElement:null, listeners:{},
  getElementById(id) { assert.ok(ids.has(id), `Unknown element ${id}`); return ids.get(id); },
  createElement:tag => new Element(tag),
  addEventListener(name, callback) { this.listeners[name] = callback; },
  querySelectorAll(selector) {
    if (selector === '[data-filter]') return filters;
    if (selector === '[data-view]') return views;
    assert.equal(selector, '[data-address]');
    return [...ids.values()].flatMap(walk).filter(element => Object.hasOwn(element.dataset, 'address'));
  }
};
const window = {ipc:{postMessage:body => messages.push(JSON.parse(body))}};
const context = vm.createContext({window, document, setTimeout:callback => timers.push(callback), clearTimeout() {}});
vm.runInContext(script, context, {filename:'desktop-inline.js'});
assert.deepEqual(messages, [{action:'ready'}]);
const get = id => ids.get(id);
const hostile = '<img src=x onerror="window.pwned=true">中文 & "quoted"';
const machine = (person, name, online, is_me) => ({person,machine:name,online,is_me,link:'direct',rtt_ms:4});
const session = (name, person, computer, agent, status, unread) => ({name,person,machine:computer,agent,status,unread,
  address:`${person}/${name}`,repo:hostile,branch:'main',doing:hostile,tier:'pull',cwd:'C:\\用户 空格'});
const directory = {team:hostile,person:'local',machine:'laptop',machines:[machine('local','laptop',true,true),machine('peer','remote',false,false)],
  sessions:[session(hostile,'local','laptop','constructor','busy',2),session('__proto__','peer','remote','__proto__','idle',9),session('offline','local','laptop','codex','offline',3)]};
window.reasonDesktop.update({directory,error:null,updated_at:1700000000000});
assert.equal(get('online-machines').textContent, '1');
assert.equal(get('active-sessions').textContent, '2');
assert.equal(get('unread-messages').textContent, '5', 'Remote unread mail must not enter the local total');
assert.equal(get('session-list').children.length, 3);
assert.equal(get('machine-list').children.length, 2);
assert.equal(get('breadcrumb-team').textContent, hostile);
assert.ok(get('session-list').textContent.includes(hostile));
assert.ok(get('session-list').textContent.includes('constructor'));
assert.ok(get('session-list').textContent.includes('__proto__'));
assert.ok(htmlWrites.every(value => value.startsWith('<svg ') && !value.includes(hostile)));
assert.equal(context.pwned, undefined);
assert.equal(window.pwned, undefined);
const copy = document.querySelectorAll('[data-address]')[0];
copy.emit('click');
assert.deepEqual(messages.at(-1), {action:'copy_address',address:directory.sessions[0].address});
copy.focus();
window.reasonDesktop.update({directory,error:null,updated_at:1700000000001});
assert.equal(document.activeElement.dataset.address, directory.sessions[0].address, 'Refresh preserves keyboard focus');
filters[1].emit('click');
assert.equal(get('session-list').children.length, 1);
get('search').value = 'missing session'; get('search').emit('input');
assert.equal(get('session-list').children.length, 0);
assert.equal(get('empty-title').textContent, 'No matching sessions');
get('search').value = ''; filters[0].emit('click');
assert.equal(get('session-list').children.length, 3);
get('refresh').emit('click'); assert.deepEqual(messages.at(-1), {action:'refresh'});
get('copy-link').emit('click'); assert.deepEqual(messages.at(-1), {action:'copy_team_link'});
window.reasonDesktop.notice(hostile);
assert.equal(get('notice').textContent, hostile); assert.equal(get('notice').hidden, false);
timers.at(-1)(); assert.equal(get('notice').hidden, true);
window.reasonDesktop.update({directory,error:hostile,updated_at:1700000000002});
assert.equal(get('connection-text').textContent, 'Reconnecting');
assert.ok(get('error-text').textContent.includes(hostile)); assert.equal(get('copy-link').disabled, true);
assert.equal(get('session-list').children.length, 3, 'Stale data remains visible during reconnect');
window.reasonDesktop.update({directory:null,error:'daemon unavailable',updated_at:0});
assert.equal(get('connection-text').textContent, 'Daemon offline');
assert.equal(get('empty-title').textContent, 'The local daemon is unavailable');
assert.equal(get('session-list').children.length, 0);
views[2].emit('click'); assert.equal(get('sessions-section').hidden, true);
assert.equal(get('machines-section').hidden, false);
console.log('PASS: desktop script renders safely, handles hostile kinds, filters, copy IPC, focus, notices and reconnects');
