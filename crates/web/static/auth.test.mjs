import { test } from 'node:test';
import assert from 'node:assert/strict';
import { authenticate } from './auth.mjs';

test('first open sets a password before exchanging it for a device token', async () => {
  const calls = [];
  const api = async (path, body) => {
    calls.push([path, body]);
    return path === '/auth/setup' ? { configured: true } : { token: 'device-token' };
  };
  assert.deepEqual(await authenticate(api, 'secret-pass', false, 'Browser'), { token: 'device-token' });
  assert.deepEqual(calls, [
    ['/auth/setup', { password: 'secret-pass' }],
    ['/auth/login', { password: 'secret-pass', device_name: 'Browser' }],
  ]);
});
test('configured hosts only log in; failed setup never issues a token', async () => {
  const calls = [];
  await authenticate(async path => { calls.push(path); return { token: 't' }; }, 'password', true, 'Browser');
  assert.deepEqual(calls, ['/auth/login']);
  await assert.rejects(authenticate(async path => { assert.equal(path, '/auth/setup'); throw new Error('409'); }, 'password', false, 'Browser'), /409/);
});
