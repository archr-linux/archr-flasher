// Arch R Flasher — Frontend Logic
// Tauri 2 IPC: all backend calls go through invoke()

// ---------------------------------------------------------------------------
// i18n
// ---------------------------------------------------------------------------
let lang = {};
const SUPPORTED_LOCALES = ['en', 'pt-BR', 'es', 'zh'];

async function initI18n() {
  try {
    const osLocale = await window.__TAURI__.core.invoke('get_locale');
    const normalized = osLocale.replace('_', '-');
    let locale = SUPPORTED_LOCALES.find(l => normalized.startsWith(l));
    if (!locale) {
      const langPart = normalized.split('-')[0];
      locale = SUPPORTED_LOCALES.find(l => l.startsWith(langPart)) || 'en';
    }

    const resp = await fetch(`i18n/${locale}.json`);
    lang = await resp.json();
  } catch (e) {
    try {
      const resp = await fetch('i18n/en.json');
      lang = await resp.json();
    } catch (_) {
      lang = {};
    }
  }

  applyI18n();
}

function t(key, replacements) {
  let text = lang[key] || key;
  if (replacements) {
    for (const [k, v] of Object.entries(replacements)) {
      text = text.replace(`{${k}}`, v);
    }
  }
  return text;
}

function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const key = el.getAttribute('data-i18n');
    if (lang[key]) el.textContent = lang[key];
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const key = el.getAttribute('data-i18n-title');
    if (lang[key]) el.title = lang[key];
  });
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------
let selectedConsole = null;
let selectedPanel = null;
let selectedDisk = null;
let imagePath = null;
let busy = false;

// ---------------------------------------------------------------------------
// DOM
// ---------------------------------------------------------------------------
const $ = (id) => document.getElementById(id);
const btnOriginal = $('btn-original');
const btnClone = $('btn-clone');
const panelSection = $('panel-section');
const panelSelect = $('panel-select');
const diskSection = $('disk-section');
const diskSelect = $('disk-select');
const flashSection = $('flash-section');
const btnFlash = $('btn-flash');
const progressSection = $('progress-section');
const progressFill = $('progress-fill');
const progressPercent = $('progress-percent');
const progressStage = $('progress-stage');
const statusEl = $('status');
const imageNameEl = $('image-name');
const imageVersionEl = $('image-version');
const confirmDialog = $('confirm-dialog');
const confirmText = $('confirm-text');

// ---------------------------------------------------------------------------
// Busy state — disables all controls during operations
// ---------------------------------------------------------------------------
function setBusy(isBusy) {
  busy = isBusy;
  const controls = [
    btnOriginal, btnClone, panelSelect, diskSelect,
    $('btn-select-file'), $('btn-download'), $('btn-refresh-disks'),
  ];
  controls.forEach(el => { if (el) el.disabled = isBusy; });
  updateFlashButton();
}

// ---------------------------------------------------------------------------
// Console selection
// ---------------------------------------------------------------------------
function selectConsole(console) {
  if (busy) return;
  selectedConsole = console;
  selectedPanel = null;

  btnOriginal.classList.toggle('active', console === 'original');
  btnClone.classList.toggle('active', console === 'clone');

  loadPanels(console);
  panelSection.style.display = '';
  diskSection.style.display = 'none';
  flashSection.style.display = 'none';
  updateFlashButton();
}

btnOriginal.addEventListener('click', () => selectConsole('original'));
btnClone.addEventListener('click', () => selectConsole('clone'));

// ---------------------------------------------------------------------------
// Panel loading
// ---------------------------------------------------------------------------
async function loadPanels(console) {
  const panels = await window.__TAURI__.core.invoke('get_panels', { console });

  panelSelect.innerHTML = `<option value="">${t('select_panel')}</option>`;

  panels.forEach(panel => {
    const opt = document.createElement('option');
    opt.value = JSON.stringify({ id: panel.id, dtb: panel.dtb });
    const suffix = panel.is_default ? ` (${t('recommended')})` : '';
    opt.textContent = panel.name + suffix;
    if (panel.is_default) opt.selected = true;
    panelSelect.appendChild(opt);
  });

  // Auto-select default
  const defaultPanel = panels.find(p => p.is_default);
  if (defaultPanel) {
    selectedPanel = defaultPanel;
    panelSelect.value = JSON.stringify({ id: defaultPanel.id, dtb: defaultPanel.dtb });
    onPanelSelected();
  }
}

panelSelect.addEventListener('change', () => {
  if (panelSelect.value) {
    selectedPanel = JSON.parse(panelSelect.value);
    onPanelSelected();
  } else {
    selectedPanel = null;
    diskSection.style.display = 'none';
    flashSection.style.display = 'none';
  }
  updateFlashButton();
});

function onPanelSelected() {
  diskSection.style.display = '';
  flashSection.style.display = '';
  refreshDisks();
}

// ---------------------------------------------------------------------------
// Disk listing
// ---------------------------------------------------------------------------
async function refreshDisks() {
  const disks = await window.__TAURI__.core.invoke('list_disks');

  diskSelect.innerHTML = `<option value="">${t('select_sd')}</option>`;
  selectedDisk = null;

  if (disks.length === 0) {
    const opt = document.createElement('option');
    opt.value = '';
    opt.textContent = t('no_sd');
    opt.disabled = true;
    diskSelect.appendChild(opt);
  } else {
    disks.forEach(disk => {
      const opt = document.createElement('option');
      opt.value = disk.device;
      opt.textContent = disk.name;
      diskSelect.appendChild(opt);
    });
  }

  updateFlashButton();
}

diskSelect.addEventListener('change', () => {
  selectedDisk = diskSelect.value || null;
  updateFlashButton();
});

$('btn-refresh-disks').addEventListener('click', refreshDisks);

// ---------------------------------------------------------------------------
// Flash button state
// ---------------------------------------------------------------------------
function updateFlashButton() {
  btnFlash.disabled = busy || !(imagePath && selectedConsole && selectedPanel && selectedDisk);
}

// ---------------------------------------------------------------------------
// File selection (local file picker)
// ---------------------------------------------------------------------------
$('btn-select-file').addEventListener('click', async () => {
  if (busy) return;
  try {
    const selected = await window.__TAURI__.dialog.open({
      filters: [{
        name: 'Arch R Image',
        extensions: ['img', 'xz']
      }]
    });

    if (selected) {
      imagePath = selected;
      const fileName = selected.split(/[/\\]/).pop();
      imageNameEl.textContent = fileName;
      imageNameEl.removeAttribute('data-i18n');
      imageNameEl.style.color = 'var(--text)';
      imageVersionEl.textContent = '';
      updateFlashButton();
    }
  } catch (e) {
    setStatus(t('error_select_file') + e, 'error');
  }
});

// ---------------------------------------------------------------------------
// Download latest (in-app download with progress)
// ---------------------------------------------------------------------------
$('btn-download').addEventListener('click', async () => {
  if (busy) return;
  setBusy(true);
  progressSection.style.display = '';
  progressFill.style.width = '0%';
  progressPercent.textContent = '0%';
  progressStage.textContent = t('checking_version');
  setStatus(t('checking_version'), '');

  try {
    const result = await window.__TAURI__.core.invoke('download_image');

    imagePath = result.path;
    imageNameEl.textContent = result.image_name;
    imageNameEl.removeAttribute('data-i18n');
    imageNameEl.style.color = 'var(--text)';
    imageVersionEl.textContent = result.version;

    if (result.cached) {
      setStatus(t('cached'), 'success');
    } else {
      setStatus(t('download_complete'), 'success');
    }

    progressFill.style.width = '100%';
    progressPercent.textContent = '100%';
    progressStage.textContent = '';

    // Hide progress after a moment
    setTimeout(() => {
      if (!busy) progressSection.style.display = 'none';
    }, 2000);
  } catch (e) {
    setStatus(translateError(e), 'error');
    progressSection.style.display = 'none';
  }

  setBusy(false);
  updateFlashButton();
});

// Download progress listener
window.__TAURI__.event.listen('download-progress', (event) => {
  const { percent, downloaded_bytes, total_bytes } = event.payload;
  progressFill.style.width = percent.toFixed(1) + '%';
  progressPercent.textContent = percent.toFixed(0) + '%';

  const dl = formatBytes(downloaded_bytes);
  const tot = formatBytes(total_bytes);
  progressStage.textContent = `${t('downloading')} ${dl} / ${tot}`;
});

// ---------------------------------------------------------------------------
// Flash
// ---------------------------------------------------------------------------
$('btn-flash').addEventListener('click', () => {
  if (busy) return;
  const diskName = diskSelect.options[diskSelect.selectedIndex].textContent;
  confirmText.textContent = t('confirm_text', { disk: diskName });
  confirmDialog.style.display = '';
});

$('btn-cancel').addEventListener('click', () => {
  confirmDialog.style.display = 'none';
});

$('btn-confirm').addEventListener('click', async () => {
  confirmDialog.style.display = 'none';
  await startFlash();
});

async function startFlash() {
  setBusy(true);
  progressSection.style.display = '';
  progressFill.style.width = '0%';
  progressPercent.textContent = '0%';
  progressStage.textContent = t('writing');
  setStatus(t('writing'), '');

  try {
    await window.__TAURI__.core.invoke('flash_image', {
      imagePath: imagePath,
      device: selectedDisk,
      panelDtb: selectedPanel.dtb,
      panelId: selectedPanel.id,
      variant: selectedConsole,
    });

    progressFill.style.width = '100%';
    progressPercent.textContent = '100%';
    progressStage.textContent = '';
    setStatus(t('done'), 'success');
  } catch (e) {
    if (e === 'cancelled') {
      setStatus(t('flash_cancelled'), '');
      progressSection.style.display = 'none';
    } else {
      setStatus(translateError(e), 'error');
    }
  }

  setBusy(false);
}

// Flash progress listener
window.__TAURI__.event.listen('flash-progress', (event) => {
  const { percent, stage } = event.payload;
  progressFill.style.width = percent.toFixed(1) + '%';
  progressPercent.textContent = percent.toFixed(0) + '%';
  progressStage.textContent = t(stage) || stage;
});

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------
function setStatus(text, type) {
  statusEl.textContent = text;
  statusEl.className = 'status' + (type ? ' ' + type : '');
}

function formatBytes(bytes) {
  if (bytes >= 1e9) return (bytes / 1e9).toFixed(1) + ' GB';
  if (bytes >= 1e6) return (bytes / 1e6).toFixed(0) + ' MB';
  return bytes + ' B';
}

function translateError(msg) {
  if (typeof msg !== 'string') msg = String(msg);
  const patterns = [
    [/cancelled|canceled/i, 'flash_cancelled'],
    [/not enough temp space/i, 'error_no_space'],
    [/device not found|removed/i, 'error_device_removed'],
    [/not a removable/i, 'error_not_removable'],
    [/no.*image.*found/i, 'error_no_image'],
    [/network|dns|connect|timeout/i, 'error_network'],
    [/checksum.*fail/i, 'error_checksum_failed'],
  ];
  for (const [regex, key] of patterns) {
    if (regex.test(msg)) return t(key);
  }
  return t('error') + ': ' + msg;
}

// ---------------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------------
async function init() {
  await initI18n();
  checkLatestVersion(); // fire-and-forget (does not block UI)
}

async function checkLatestVersion() {
  try {
    const release = await window.__TAURI__.core.invoke('check_latest_release');
    imageVersionEl.textContent = release.version;
    setStatus(t('latest_version', { version: release.version }), '');
  } catch (_) {
    // offline or error — ignore silently
  }
}

init();
