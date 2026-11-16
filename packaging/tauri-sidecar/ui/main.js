// UI for the xkrd sidecar: polls the Rust commands node_info and node_status every 2 s.
const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
let busy = false;
let formFilled = false;

function text(id, value) {
  $(id).textContent = value === undefined || value === null || value === '' ? '–' : String(value);
}

function fmtInt(n) {
  return typeof n === 'number' ? n.toLocaleString('en-US') : n;
}

function fmtRate(h) {
  if (typeof h !== 'number') return '–';
  const units = ['H/s', 'kH/s', 'MH/s', 'GH/s'];
  let i = 0;
  while (h >= 1000 && i < units.length - 1) { h /= 1000; i++; }
  return `${h.toFixed(i === 0 ? 0 : 2)} ${units[i]}`;
}

function showInfo(info) {
  const phase = $('phase');
  phase.textContent = busy && info.phase === 'running' ? 'restarting' : info.phase;
  phase.className = `badge ${info.phase}`;
  $('detail').hidden = !info.detail;
  $('detail').textContent = info.detail || '';
  text('data-dir', info.data_dir);
  text('log-file', info.log_file);
  text('rpc', `127.0.0.1:${info.rpc_port}`);
  const hyperOn = info.settings.hyper && info.hyper_available;
  text('hyper', hyperOn ? 'on' : info.settings.hyper ? 'off (bridge not bundled)' : 'off');

  const threads = $('threads');
  threads.max = info.cores;
  text('threads-hint', `default ${info.default_threads} of ${info.cores} cores`);
  if (!formFilled) {
    $('address').value = info.settings.mining.address || '';
    threads.value = info.settings.mining.threads || info.default_threads;
    formFilled = true;
  }
  const mining = info.settings.mining.enabled;
  $('mine-start').textContent = mining ? 'Apply' : 'Start mining';
  $('mine-stop').disabled = busy || !mining;
  $('mine-start').disabled = busy;
}

function showStatus(s) {
  if (!s) {
    for (const id of ['network', 'height', 'network-height', 'sync', 'peers', 'version']) text(id, null);
    $('sync-bar').value = 0;
    return;
  }
  text('network', s.network);
  text('height', fmtInt(s.height));
  text('network-height', fmtInt(s.network_height));
  const progress = Math.min(1, Number(s.sync_progress) || 0);
  $('sync-bar').value = progress;
  text('sync', s.synced ? 'synchronized' : `${(progress * 100).toFixed(2)} %`);
  const p = s.peers || {};
  text('peers', `${fmtInt(p.total)} (legacy ${p.legacy_outgoing} out / ${p.legacy_incoming} in, Hyperswarm ${p.hyper})`);
  text('version', s.version);

  const m = s.mining;
  if (!m || !m.active) {
    text('mining-state', 'off');
    for (const id of ['hashrate', 'total-hashes', 'mining-block', 'blocks-found', 'mining-last-error']) text(id, null);
    return;
  }
  text('mining-state', s.synced ? `mining with ${m.threads} threads` : `waiting for sync (${m.threads} threads)`);
  text('hashrate', fmtRate(m.hashrate));
  text('total-hashes', fmtInt(m.total_hashes));
  text('mining-block', `${fmtInt(m.height)} / ${fmtInt(m.difficulty)}`);
  text('blocks-found', `${fmtInt(m.blocks_found)} accepted, ${fmtInt(m.blocks_rejected)} rejected`);
  text('mining-last-error', m.last_error);
}

async function refresh() {
  try {
    const info = await invoke('node_info');
    showInfo(info);
    showStatus(info.phase === 'running' ? await invoke('node_status').catch(() => null) : null);
  } catch (e) {
    $('detail').hidden = false;
    $('detail').textContent = String(e);
  }
}

async function applyMining(enabled) {
  busy = true;
  $('mining-error').hidden = true;
  await refresh();
  try {
    await invoke('set_mining', {
      enabled,
      address: $('address').value.trim(),
      threads: Number($('threads').value) || 0,
    });
  } catch (e) {
    $('mining-error').hidden = false;
    $('mining-error').textContent = String(e);
  } finally {
    busy = false;
    await refresh();
  }
}

$('mining-form').addEventListener('submit', (ev) => {
  ev.preventDefault();
  applyMining(true);
});
$('mine-stop').addEventListener('click', () => applyMining(false));

refresh();
setInterval(refresh, 2000);
