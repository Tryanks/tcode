// Password authentication belongs to the serving browser origin. Native clients
// continue exchanging six-digit codes through /pair.
export async function authenticate(api, password, configured, deviceName) {
  if (!configured) await api('/auth/setup', { password });
  return api('/auth/login', { password, device_name: deviceName });
}

export async function authorizeBrowser() {
  const api = async (path, body) => {
    const response = await fetch(path, {
      method: body ? 'POST' : 'GET',
      headers: { 'Content-Type': 'application/json' },
      body: body ? JSON.stringify(body) : undefined,
      cache: 'no-store',
    });
    if (!response.ok) throw new Error(String(response.status));
    return response.json();
  };
  const state = await api('/auth/state');
  if (state.mode !== 'password') return;
  document.documentElement.dataset.authMode = 'password';
  // A code fragment must never sign this browser in, even if it is valid for
  // a native device. Tokens previously paired by this browser remain usable.
  history.replaceState(null, '', location.pathname + location.search);
  let hosts = [];
  try { hosts = JSON.parse(localStorage.getItem('tcode.hosts') || '[]'); } catch { /* damaged site data */ }
  const last = localStorage.getItem('tcode.last_host');
  const saved = hosts.find(host => host.host_id === last && host.origin === location.origin);
  if (saved) return; // The normal hello verifies it; rejection returns to login.

  const zh = navigator.language.toLowerCase().startsWith('zh');
  const translations = __AUTH_LOCALES__;
  const text = translations[zh ? 'zh-CN' : 'en'];
  const loading = document.getElementById('loading');
  loading.hidden = true;
  const form = document.createElement('form');
  form.id = 'auth';
  const title = document.createElement('h1');
  const description = document.createElement('p');
  description.textContent = text.description;
  const field = (name, caption) => {
    const label = document.createElement('label');
    label.textContent = caption;
    const input = document.createElement('input');
    input.type = 'password'; input.name = name; input.required = true;
    input.maxLength = 1024;
    input.autocomplete = state.configured ? 'current-password' : 'new-password';
    label.append(input); form.append(label);
    return input;
  };
  form.append(title, description);
  const password = field('password', text.password);
  const confirm = field('confirm', text.confirm);
  const error = document.createElement('p'); error.role = 'alert';
  const button = document.createElement('button'); button.type = 'submit';
  const render = () => {
    title.textContent = button.textContent = state.configured ? text.login : text.setup;
    confirm.parentElement.hidden = state.configured;
    confirm.required = !state.configured;
    password.placeholder = state.configured ? '' : text.minimum;
  };
  form.append(error, button); document.body.append(form); render(); password.focus();
  await new Promise(resolve => {
    form.addEventListener('submit', async event => {
      event.preventDefault(); error.textContent = '';
      if (!state.configured && [...password.value].length < 8) { error.textContent = text.minimum; return; }
      if (!state.configured && password.value !== confirm.value) { error.textContent = text.mismatch; return; }
      button.disabled = true;
      try {
        const paired = await authenticate(async (path, body) => {
          const result = await api(path, body);
          if (path === '/auth/setup') state.configured = true;
          return result;
        }, password.value, state.configured, 'Browser');
        const host = { host_id: paired.host_id, name: paired.host_name, token: paired.token, origin: location.origin };
        hosts = hosts.filter(saved => saved.host_id !== host.host_id); hosts.push(host);
        localStorage.setItem('tcode.hosts', JSON.stringify(hosts));
        localStorage.setItem('tcode.last_host', host.host_id);
        form.remove(); loading.hidden = false; resolve();
      } catch (failure) {
        if (failure.message === '409') { state.configured = true; error.textContent = text.conflict; }
        else error.textContent = failure.message === '403' ? text.failed : text.network;
        render(); button.disabled = false;
      }
    });
  });
}
