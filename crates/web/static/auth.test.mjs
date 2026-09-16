import { test } from 'node:test';
import assert from 'node:assert/strict';
import { authenticate, deviceIdentity } from './auth.mjs';

test('first open sets a password before exchanging it for a device token', async () => {
  const calls = [];
  const api = async (path, body) => {
    calls.push([path, body]);
    return path === '/auth/setup' ? { configured: true } : { token: 'device-token' };
  };
  const device = { device_id: 'browser-id', device_name: 'Browser' };
  assert.deepEqual(await authenticate(api, 'secret-pass', false, device), { token: 'device-token' });
  assert.deepEqual(calls, [
    ['/auth/setup', { password: 'secret-pass' }],
    ['/auth/login', { password: 'secret-pass', device_id: 'browser-id', device_name: 'Browser' }],
  ]);
});
test('a browser mints its device id once and reuses it on later logins', () => {
  const stored = new Map();
  const storage = { getItem: key => stored.get(key) ?? null, setItem: (key, value) => stored.set(key, value) };
  const first = deviceIdentity(storage, () => 'minted-id');
  assert.deepEqual(first, { device_id: 'minted-id', device_name: 'Browser' });
  assert.deepEqual(deviceIdentity(storage, () => { throw new Error('must reuse'); }), first);
});
test('configured hosts only log in; failed setup never issues a token', async () => {
  const calls = [];
  const device = { device_id: 'browser-id', device_name: 'Browser' };
  await authenticate(async path => { calls.push(path); return { token: 't' }; }, 'password', true, device);
  assert.deepEqual(calls, ['/auth/login']);
  await assert.rejects(authenticate(async path => { assert.equal(path, '/auth/setup'); throw new Error('409'); }, 'password', false, device), /409/);
});
