'use strict';

const { invoke } = window.__TAURI__.tauri;

// ── Состояние ──────────────────────────────────────────────────────────────
let accounts = [];
let currentAccountId = null;
let accountUnreadCounts = {}; // { id: count }
let currentFolder = 'INBOX';
let allEmails = [];
let currentEmailId = null;
let composeFiles = []; // File objects для вложений
let syncedCount = 0;   // сколько писем загружено с сервера для текущей папки (для пагинации)
let currentDraftId = null;   // id черновика, который сейчас редактируется
let draftAutoSaveTimer = null; // таймер автосохранения черновика
let renameFolderOldName = null; // имя папки, которую переименовываем
let dbOffset = 0;      // сколько писем уже загружено из БД (пагинация по БД)
const PAGE_SIZE = 50;  // размер страницы
let selectedIds = new Set(); // выбранные письма для массовых операций
let _syncing = false;        // true пока идёт sync_folder
let _syncNewCount = 0;       // сколько новых писем пришло в текущем сеансе синка
let _dragCounter = 0;        // счётчик dragenter/dragleave для DnD в compose

const FOLDER_NAMES = {
    'INBOX':  'Входящие',
    'Sent':   'Отправленные',
    'Drafts': 'Черновики',
    'Starred':'Помечённые',
    'Spam':   'Спам',
    'Trash':  'Корзина',
};

const PRESETS = {
    mailru: { imapHost: 'imap.mail.ru',     imapPort: 993, smtpHost: 'smtp.mail.ru',     smtpPort: 465 },
    yandex: { imapHost: 'imap.yandex.ru',   imapPort: 993, smtpHost: 'smtp.yandex.ru',   smtpPort: 465 },
    gmail:  { imapHost: 'imap.gmail.com',   imapPort: 993, smtpHost: 'smtp.gmail.com',   smtpPort: 465 },
    other:  { imapHost: '',                 imapPort: 993, smtpHost: '',                 smtpPort: 465 },
};

// ── Тема ───────────────────────────────────────────────────────────────────
function applyTheme(theme) {
    document.documentElement.setAttribute('data-theme', theme);
    // обновляем кнопки в панели настроек
    document.querySelectorAll('.theme-opt').forEach(b => b.classList.remove('active'));
    const activeBtn = document.getElementById(theme === 'light' ? 'themeOptLight' : 'themeOptDark');
    if (activeBtn) activeBtn.classList.add('active');
}

function setTheme(theme) {
    applyTheme(theme);
    localStorage.setItem('mail-theme', theme);
}

// ── Автопроверка писем ─────────────────────────────────────────────────────
let _autoSyncTimer = null;
const _foldersRefreshed = new Set(); // папки с IMAP обновляются 1 раз за сессию на аккаунт

function applyAutoSync(minutes) {
    if (_autoSyncTimer) { clearInterval(_autoSyncTimer); _autoSyncTimer = null; }
    const ms = parseInt(minutes) * 60 * 1000;
    if (ms > 0) {
        _autoSyncTimer = setInterval(() => syncAllAccountsBackground(), ms);
    }
}

async function syncAllAccountsBackground() {
    if (_syncing || accounts.length === 0) return;
    const notifEnabled = localStorage.getItem('mail-notifications') !== 'false';
    const notifDuration = parseInt(localStorage.getItem('mail-notif-duration') || '5');
    const saveBase = localStorage.getItem('mail-attach-path') || '';
    for (const a of accounts) {
        try {
            const newItems = await invoke('sync_folder', { accountId: a.id, folder: 'INBOX', offset: 0 });
            if (newItems && newItems.length > 0) {
                if (saveBase) invoke('auto_save_attachments', { emailIds: newItems.map(n => n.id), saveBase }).catch(() => {});
                if (notifEnabled) {
                    playMailSound();
                    for (const item of newItems.slice(0, 3)) {
                        invoke('show_mail_notification', {
                            fromName: shortAddr(item.from_addr),
                            subject: item.subject || '(без темы)',
                            emailId: item.id,
                            durationSecs: notifDuration,
                        }).catch(() => {});
                    }
                }
                // если это активный аккаунт и открыт INBOX — обновляем список
                if (a.id === currentAccountId && currentFolder === 'INBOX') {
                    await loadEmails();
                }
            }
        } catch (e) {
            console.warn(`Auto-sync [${a.email}]:`, e);
        }
    }
    await refreshAllAccountBadges();
}

// ── Настройки ─────────────────────────────────────────────────────────────
async function openSettings() {
    // Путь к БД
    try {
        const path = await invoke('get_data_dir_path');
        document.getElementById('settingsDataDir').textContent = path;
    } catch (_) {}

    // Путь к вложениям: из localStorage или программный дефолт
    const savedAttach = localStorage.getItem('mail-attach-path') || '';
    const attachInput = document.getElementById('settingsAttachPath');
    if (savedAttach) {
        attachInput.value = savedAttach;
    } else {
        try {
            const def = await invoke('get_default_attach_path');
            attachInput.placeholder = def;
        } catch (_) {}
        attachInput.value = '';
    }

    // Тема
    const theme = localStorage.getItem('mail-theme') || 'light';
    applyTheme(theme);

    // Подпись
    document.getElementById('settingsSignature').value =
        localStorage.getItem('mail-signature') || '';

    // Таймер
    const interval = localStorage.getItem('mail-autosync') || '0';
    document.getElementById('settingsAutoSync').value = interval;

    // Уведомления
    const notifEnabled = localStorage.getItem('mail-notifications') !== 'false';
    document.getElementById('settingsNotifications').checked = notifEnabled;
    const notifDuration = localStorage.getItem('mail-notif-duration') || '5';
    document.getElementById('settingsNotifDuration').value = notifDuration;
    document.getElementById('settingsNotifDurationRow').style.display = notifEnabled ? '' : 'none';

    // Автозапуск с Windows
    try {
        const autostart = await invoke('get_autostart');
        document.getElementById('settingsAutostart').checked = autostart;
    } catch { document.getElementById('settingsAutostart').checked = false; }

    // Резервное копирование
    try {
        const bs = await invoke('get_backup_settings');
        document.getElementById('settingsBackupEnabled').checked = bs.enabled;
        document.getElementById('settingsBackupInterval').value = String(bs.interval_days);
        document.getElementById('backupIntervalRow').style.display = bs.enabled ? 'flex' : 'none';
        const lastEl = document.getElementById('settingsLastBackup');
        if (bs.last_backup) {
            const d = new Date(bs.last_backup);
            lastEl.textContent = 'Последняя копия: ' + d.toLocaleString('ru-RU');
        } else {
            lastEl.textContent = 'Резервных копий ещё нет';
        }
    } catch { /* нет критично */ }

    document.getElementById('settingsModal').style.display = 'flex';
}

function closeSettings() {
    document.getElementById('settingsModal').style.display = 'none';
}

function closeSettingsOverlay(e) {
    if (e.target === document.getElementById('settingsModal')) closeSettings();
}

async function openAbout() {
    try {
        const ver = await window.__TAURI__.app.getVersion();
        document.getElementById('aboutVersion').textContent = 'Версия ' + ver;
    } catch (e) {}
    document.getElementById('aboutModal').style.display = 'flex';
}
function closeAbout() {
    document.getElementById('aboutModal').style.display = 'none';
}
function closeAboutOverlay(e) {
    if (e.target === document.getElementById('aboutModal')) closeAbout();
}
function openAboutLink(e, url) {
    e.preventDefault();
    window.__TAURI__.shell.open(url);
}

// ── Резервное копирование ─────────────────────────────────────────────────────
function onNotifEnabledChange() {
    const enabled = document.getElementById('settingsNotifications').checked;
    document.getElementById('settingsNotifDurationRow').style.display = enabled ? '' : 'none';
}

function onBackupEnabledChange() {
    const enabled = document.getElementById('settingsBackupEnabled').checked;
    document.getElementById('backupIntervalRow').style.display = enabled ? 'flex' : 'none';
}

async function doBackup() {
    const btn = document.getElementById('backupBtn');
    btn.disabled = true;
    btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Создание...';
    try {
        const path = await invoke('backup_database');
        const lastEl = document.getElementById('settingsLastBackup');
        lastEl.textContent = 'Последняя копия: ' + new Date().toLocaleString('ru-RU');
        showBackupToast(true, 'Резервная копия создана');
    } catch (e) {
        showBackupToast(false, 'Ошибка: ' + e);
    } finally {
        btn.disabled = false;
        btn.innerHTML = '<i class="fas fa-archive"></i> Создать копию';
    }
}

async function doRestore() {
    // Открываем диалог сразу в папке с резервными копиями
    let defaultPath;
    try { defaultPath = await invoke('get_backups_dir_path'); } catch (_) {}

    let filePath;
    try {
        filePath = await window.__TAURI__.dialog.open({
            title: 'Выберите резервную копию для восстановления',
            filters: [{ name: 'Резервная копия базы данных', extensions: ['db'] }],
            multiple: false,
            defaultPath,
        });
    } catch (e) { return; }
    if (!filePath) return;

    const ok = confirm(
        'Текущая база данных будет заменена выбранным файлом.\n' +
        'Для применения потребуется перезапуск приложения.\n\n' +
        'Продолжить?'
    );
    if (!ok) return;

    try {
        await invoke('restore_database', { sourcePath: filePath });
        const restart = confirm('Резервная копия готова к применению. Перезапустить приложение сейчас?');
        if (restart) {
            await invoke('restart_app');
        }
    } catch (e) {
        alert('Ошибка восстановления: ' + e);
    }
}

function showBackupToast(success, msg) {
    let toast = document.getElementById('backupToast');
    if (!toast) {
        toast = document.createElement('div');
        toast.id = 'backupToast';
        toast.className = 'attach-toast';
        document.body.appendChild(toast);
    }
    toast.className = 'attach-toast ' + (success ? 'success' : 'error');
    toast.innerHTML = success
        ? '<i class="fas fa-check-circle"></i> ' + msg
        : '<i class="fas fa-exclamation-circle"></i> ' + msg;
    toast.classList.add('visible');
    setTimeout(() => toast.classList.remove('visible'), 3500);
}

async function openDataDirExplorer() {
    try { await invoke('open_data_dir'); } catch (e) { alert('Ошибка: ' + e); }
}

async function resetSyncCache() {
    if (!currentAccountId) { alert('Выберите аккаунт'); return; }
    if (!confirm('Сбросить кеш синхронизации для текущего аккаунта?\nПри следующей проверке будет сделана начальная загрузка с корректным UID.')) return;
    try {
        await invoke('reset_folder_state', { accountId: currentAccountId });
        alert('Кеш синхронизации сброшен. Закройте настройки и нажмите «Проверить почту».');
    } catch (e) {
        alert('Ошибка: ' + e);
    }
}

async function browseAttachPath() {
    try {
        const selected = await window.__TAURI__.dialog.open({ directory: true, multiple: false });
        if (selected) {
            document.getElementById('settingsAttachPath').value = selected;
        }
    } catch (e) { alert('Ошибка: ' + e); }
}

function saveSettings() {
    // Тема
    const theme = document.documentElement.getAttribute('data-theme') || 'light';
    localStorage.setItem('mail-theme', theme);

    // Путь вложений
    const attachPath = document.getElementById('settingsAttachPath').value.trim();
    if (attachPath) {
        localStorage.setItem('mail-attach-path', attachPath);
    } else {
        localStorage.removeItem('mail-attach-path');
    }

    // Подпись
    localStorage.setItem('mail-signature', document.getElementById('settingsSignature').value);

    // Таймер
    const interval = document.getElementById('settingsAutoSync').value;
    localStorage.setItem('mail-autosync', interval);
    applyAutoSync(interval);

    // Уведомления
    const notifEnabled = document.getElementById('settingsNotifications').checked;
    localStorage.setItem('mail-notifications', notifEnabled ? 'true' : 'false');
    const notifDuration = document.getElementById('settingsNotifDuration').value;
    localStorage.setItem('mail-notif-duration', notifDuration);

    // Автозапуск с Windows
    const autostartEnabled = document.getElementById('settingsAutostart').checked;
    invoke('set_autostart', { enable: autostartEnabled }).catch(e => console.warn('autostart:', e));

    // Резервное копирование — только сохраняем настройки таймера
    const backupEnabled = document.getElementById('settingsBackupEnabled').checked;
    const backupInterval = parseInt(document.getElementById('settingsBackupInterval').value, 10);
    invoke('set_backup_settings', { enabled: backupEnabled, intervalDays: backupInterval }).catch(() => {});

    closeSettings();
}

// Двойной «дин» — как в Outlook, без звукового файла
function playMailSound() {
    try {
        const ctx = new (window.AudioContext || window.webkitAudioContext)();
        const ding = (freq, t, dur) => {
            const osc = ctx.createOscillator();
            const gain = ctx.createGain();
            osc.connect(gain);
            gain.connect(ctx.destination);
            osc.type = 'sine';
            osc.frequency.value = freq;
            gain.gain.setValueAtTime(0, t);
            gain.gain.linearRampToValueAtTime(0.22, t + 0.01);
            gain.gain.exponentialRampToValueAtTime(0.001, t + dur);
            osc.start(t);
            osc.stop(t + dur);
        };
        ding(880,  ctx.currentTime,        0.45); // ля
        ding(1108, ctx.currentTime + 0.18, 0.45); // до#
    } catch (_) {}
}

// Открываем ссылки из iframe-писем в браузере по умолчанию
window.addEventListener('message', function(e) {
    if (e.data && e.data.type === 'mailLink' && e.data.url) {
        window.__TAURI__.shell.open(e.data.url);
    }
});

// ── Инициализация ──────────────────────────────────────────────────────────
document.addEventListener('DOMContentLoaded', async () => {
    // Восстанавливаем тему
    const savedTheme = localStorage.getItem('mail-theme') || 'light';
    applyTheme(savedTheme);

    // Запускаем автопроверку если была настроена
    const savedInterval = localStorage.getItem('mail-autosync') || '0';
    applyAutoSync(savedInterval);

    // Закрывать дропдауны по клику вне
    document.addEventListener('click', e => {
        if (!e.target.closest('.addr-input-wrap')) closeAddrDropdowns();
        if (!e.target.closest('.group-assign-wrap')) {
            const dd = document.getElementById('groupAssignDropdown');
            if (dd) dd.style.display = 'none';
        }
    });

    // Прогреваем PowerShell/WPF заранее — чтобы первое уведомление появлялось быстро
    invoke('prewarm_powershell').catch(() => {});

    // Подписываемся на клик по уведомлению — открываем нужное письмо
    window.__TAURI__.event.listen('open-email-from-notification', (e) => {
        const emailId = e.payload;
        if (emailId) openEmail(emailId);
    });

    // Уведомление → кнопка ⚙ настроек
    window.__TAURI__.event.listen('open-notification-settings', () => {
        openSettings().then(() => {
            // Прокручиваем к разделу уведомлений
            const el = document.getElementById('settingsNotifSection');
            if (el) setTimeout(() => el.scrollIntoView({ behavior: 'smooth', block: 'start' }), 100);
        });
    });

    // Трей → "Проверить почту"
    window.__TAURI__.event.listen('tray-check-mail', () => {
        syncCurrentFolder();
    });

    // Пошаговое отображение новых писем по мере их загрузки с сервера
    window.__TAURI__.event.listen('email-received', ({ payload: email }) => {
        if (!_syncing) return;
        if (email.folder !== currentFolder) return;

        // Не дублировать, если письмо уже есть в списке
        if (allEmails.some(e => e.id === email.id)) return;

        // Добавляем в начало массива
        allEmails.unshift(email);
        _syncNewCount++;

        // Обновляем статус-строку по ходу приёма
        const el = document.getElementById('sbText');
        if (el) el.textContent = `Получение... (${_syncNewCount})`;

        // Вставляем в DOM: убираем заглушку, добавляем элемент сверху
        const list = document.getElementById('emailList');
        const empty = list.querySelector('.empty-state');
        if (empty) {
            list.innerHTML = '';
            list.insertAdjacentHTML('beforeend', `
                <div class="load-more-wrap" id="loadMoreWrap">
                    <button class="load-more-btn" id="loadMoreBtn" onclick="loadMoreEmails()">
                        <i class="fas fa-chevron-down"></i> Загрузить ещё
                    </button>
                </div>`);
        }
        const wrap = document.createElement('div');
        wrap.innerHTML = emailItemHtml(email);
        list.prepend(wrap.firstElementChild);
    });

    // ── Drag-and-drop файлов в окно написания письма ──────────────────────────
    // Используем счётчик dragenter/dragleave на document, чтобы обойти проблему
    // с contenteditable div-ом, который поглощает события перетаскивания.
    document.addEventListener('dragenter', e => {
        if (document.getElementById('composeModal').style.display === 'none') return;
        if (!e.dataTransfer || !e.dataTransfer.types.includes('Files')) return;
        e.preventDefault();
        _dragCounter++;
        document.querySelector('.compose-attach-zone')?.classList.add('drag-over');
    }, false);
    document.addEventListener('dragleave', e => {
        if (document.getElementById('composeModal').style.display === 'none') return;
        _dragCounter--;
        if (_dragCounter <= 0) {
            _dragCounter = 0;
            document.querySelector('.compose-attach-zone')?.classList.remove('drag-over');
        }
    }, false);
    document.addEventListener('dragover', e => {
        if (document.getElementById('composeModal').style.display === 'none') return;
        if (!e.dataTransfer || !e.dataTransfer.types.includes('Files')) return;
        e.preventDefault();
    }, false);
    document.addEventListener('drop', e => {
        if (document.getElementById('composeModal').style.display === 'none') return;
        e.preventDefault();
        _dragCounter = 0;
        document.querySelector('.compose-attach-zone')?.classList.remove('drag-over');
        const files = Array.from(e.dataTransfer.files || []);
        if (files.length === 0) return;
        files.forEach(f => composeFiles.push(f));
        renderComposeAttachList();
    }, false);

    accounts = await invoke('get_accounts');
    if (accounts.length === 0) {
        showSetup();
    } else {
        showApp();
        refreshAllAccountBadges(); // загружаем счётчики всех аккаунтов сразу
        selectAccount(accounts[0].id);
        // Автосинк при запуске
        syncCurrentFolder();
    }
});

// ── Экран настройки ────────────────────────────────────────────────────────
function showSetup() {
    document.getElementById('setupScreen').style.display = 'flex';
    document.getElementById('mainApp').style.display = 'none';
    // Кнопка "Вернуться" видна только если уже есть хотя бы один аккаунт
    const cancelBtn = document.getElementById('setupCancelBtn');
    if (cancelBtn) cancelBtn.style.display = accounts.length > 0 ? 'flex' : 'none';
}

function showApp() {
    document.getElementById('setupScreen').style.display = 'none';
    document.getElementById('mainApp').style.display = 'flex';
    renderAccountList();
}

function applyPreset(key) {
    const p = PRESETS[key];
    document.getElementById('setupImapHost').value = p.imapHost;
    document.getElementById('setupImapPort').value = p.imapPort;
    document.getElementById('setupSmtpHost').value = p.smtpHost;
    document.getElementById('setupSmtpPort').value = p.smtpPort;
    if (key !== 'other') {
        document.getElementById('advancedSettings').open = false;
    } else {
        document.getElementById('advancedSettings').open = true;
    }
    // Автозаполнение хоста по email
    const email = document.getElementById('setupEmail').value;
    if (key === 'other' && email.includes('@')) {
        const domain = email.split('@')[1];
        document.getElementById('setupImapHost').value = 'imap.' + domain;
        document.getElementById('setupSmtpHost').value = 'smtp.' + domain;
    }
}

async function saveAccount() {
    const email    = document.getElementById('setupEmail').value.trim();
    const name     = document.getElementById('setupName').value.trim() || email;
    const password = document.getElementById('setupPassword').value;
    const imapHost = document.getElementById('setupImapHost').value.trim();
    const imapPort = parseInt(document.getElementById('setupImapPort').value) || 993;
    const smtpHost = document.getElementById('setupSmtpHost').value.trim();
    const smtpPort = parseInt(document.getElementById('setupSmtpPort').value) || 465;

    if (!email || !password || !imapHost || !smtpHost) {
        showSetupError('Заполните все обязательные поля');
        return;
    }

    const btn = document.getElementById('setupSaveBtn');
    btn.disabled = true;
    btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Проверка подключения...';
    hideSetupError();

    try {
        await invoke('save_account', { email, name, password, imapHost, imapPort, smtpHost, smtpPort });
        accounts = await invoke('get_accounts');
        showApp();
        const acc = accounts.find(a => a.email === email);
        if (acc) {
            selectAccount(acc.id);
            syncFolder('INBOX');
        }
    } catch (e) {
        showSetupError('Ошибка: ' + e);
    } finally {
        btn.disabled = false;
        btn.innerHTML = '<i class="fas fa-plus"></i> Добавить аккаунт';
    }
}

function showSetupError(msg) {
    const el = document.getElementById('setupError');
    el.textContent = msg;
    el.style.display = 'block';
}
function hideSetupError() {
    document.getElementById('setupError').style.display = 'none';
}

// ── Аккаунты ───────────────────────────────────────────────────────────────
function renderAccountList() {
    const el = document.getElementById('accountList');
    el.innerHTML = accounts.map(a => {
        const uc = accountUnreadCounts[a.id] || 0;
        const badge = uc > 0 ? `<span class="account-unread">${uc > 999 ? '999+' : uc}</span>` : '';
        return `
        <div class="account-item ${a.id === currentAccountId ? 'active' : ''}"
             onclick="selectAccount(${a.id})">
            <div class="account-avatar">${a.email[0].toUpperCase()}</div>
            <span class="account-email">${a.email}</span>
            ${badge}
            <span class="account-edit" onclick="openEditAccount(event,${a.id})" title="Настройки">
                <i class="fas fa-pen"></i>
            </span>
            <span class="account-del" onclick="removeAccount(event, ${a.id})" title="Удалить">
                <i class="fas fa-times"></i>
            </span>
        </div>`;
    }).join('');
}

async function selectAccount(id) {
    currentAccountId = id;
    renderAccountList();
    // Сначала загружаем письма — они имеют приоритет над мьютексом БД
    await selectFolder(currentFolder);
    // После того как get_emails освободил мьютекс — запускаем остальное
    updateUnreadBadge();
    loadCustomFolders();
    refreshCustomFolders();
}

async function removeAccount(e, id) {
    e.stopPropagation();
    if (!await window.__TAURI__.dialog.ask('Удалить аккаунт и все его письма?', { title: 'Подтверждение', type: 'warning' })) return;
    await invoke('delete_account', { accountId: id });
    accounts = await invoke('get_accounts');
    if (accounts.length === 0) {
        showSetup();
    } else {
        currentAccountId = accounts[0].id;
        renderAccountList();
        loadEmails();
    }
}

// ── Редактирование аккаунта ────────────────────────────────────────────────
let _editAccountId = null;

function openEditAccount(e, id) {
    e.stopPropagation();
    const a = accounts.find(x => x.id === id);
    if (!a) return;
    _editAccountId = id;

    document.getElementById('editAccountEmail').value    = a.email;
    document.getElementById('editAccountName').value     = a.name;
    document.getElementById('editAccountPassword').value = '';   // не показываем текущий
    document.getElementById('editImapHost').value = a.imap_host;
    document.getElementById('editImapPort').value = a.imap_port;
    document.getElementById('editSmtpHost').value = a.smtp_host;
    document.getElementById('editSmtpPort').value = a.smtp_port;
    document.getElementById('editAccountError').style.display = 'none';
    document.getElementById('editAdvancedSettings').open = false;

    // Сбрасываем иконку глаза
    document.getElementById('editPasswordEyeIcon').className = 'fas fa-eye';
    document.getElementById('editAccountPassword').type = 'password';

    document.getElementById('editAccountModal').style.display = 'flex';
}

function closeEditAccount() {
    document.getElementById('editAccountModal').style.display = 'none';
    _editAccountId = null;
}

function closeEditAccountOverlay(e) {
    if (e.target === document.getElementById('editAccountModal')) closeEditAccount();
}

function toggleEditPasswordVisibility() {
    const inp = document.getElementById('editAccountPassword');
    const icon = document.getElementById('editPasswordEyeIcon');
    if (inp.type === 'password') {
        inp.type = 'text';
        icon.className = 'fas fa-eye-slash';
    } else {
        inp.type = 'password';
        icon.className = 'fas fa-eye';
    }
}

async function saveEditAccount() {
    if (!_editAccountId) return;
    const a = accounts.find(x => x.id === _editAccountId);
    if (!a) return;

    const name     = document.getElementById('editAccountName').value.trim() || a.email;
    const newPwd   = document.getElementById('editAccountPassword').value;
    const password = newPwd || a.password;  // если поле пустое — используем текущий пароль
    const imapHost = document.getElementById('editImapHost').value.trim();
    const imapPort = parseInt(document.getElementById('editImapPort').value) || 993;
    const smtpHost = document.getElementById('editSmtpHost').value.trim();
    const smtpPort = parseInt(document.getElementById('editSmtpPort').value) || 465;

    const btn = document.getElementById('editAccountSaveBtn');
    btn.disabled = true;
    btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Проверка подключения...';
    document.getElementById('editAccountError').style.display = 'none';

    try {
        await invoke('update_account', {
            accountId: _editAccountId, name, password, imapHost, imapPort, smtpHost, smtpPort,
        });
        // Обновляем локальный массив
        accounts = await invoke('get_accounts');
        closeEditAccount();
        renderAccountList();
    } catch (e) {
        const err = document.getElementById('editAccountError');
        err.textContent = 'Ошибка: ' + e;
        err.style.display = 'block';
    } finally {
        btn.disabled = false;
        btn.innerHTML = '<i class="fas fa-save"></i> Сохранить';
    }
}

// ── Папки ──────────────────────────────────────────────────────────────────
function selectFolder(folder) {
    currentFolder = folder;
    currentEmailId = null;
    syncedCount = 0;
    dbOffset = 0;
    selectedIds.clear();
    updateSelectToolbar();

    document.querySelectorAll('.folder-item').forEach(el => {
        el.classList.toggle('active', el.dataset.folder === folder);
    });

    const title = FOLDER_NAMES[folder] || folder;
    document.getElementById('folderTitle').textContent = title;
    const clearBtn = document.getElementById('clearTrashBtn');
    if (clearBtn) clearBtn.style.display = folder === 'Trash' ? 'inline-flex' : 'none';
    document.getElementById('emailViewPanel').innerHTML =
        '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';

    return loadEmails();
}

async function loadEmails() {
    if (!currentAccountId) return;
    dbOffset = 0;
    document.getElementById('emailList').innerHTML =
        '<div class="empty-state"><i class="fas fa-spinner fa-spin"></i><p>Загрузка...</p></div>';

    try {
        if (currentFolder === 'Starred') {
            // Помечённые — объединяем INBOX + Sent, фильтруем; берём сразу больше
            const [inbox, sent] = await Promise.all([
                invoke('get_emails', { accountId: currentAccountId, folder: 'INBOX', limit: 2000, offset: 0 }),
                invoke('get_emails', { accountId: currentAccountId, folder: 'Sent',  limit: 2000, offset: 0 }),
            ]);
            allEmails = [...inbox, ...sent].filter(e => e.is_starred);
            // date_ts уже integer — сортировка без Date.parse
            allEmails.sort((a, b) => b.date_ts - a.date_ts);
            dbOffset = allEmails.length;
        } else {
            // Быстрая первая страница: только PAGE_SIZE писем
            const page = await invoke('get_emails', {
                accountId: currentAccountId,
                folder: currentFolder,
                limit: PAGE_SIZE,
                offset: 0,
            });
            allEmails = page;
            dbOffset = page.length;
        }
        if (syncedCount === 0) syncedCount = allEmails.length;
        // SQL уже вернул в нужном порядке (date_ts DESC) — JS-сортировка не нужна
        renderEmailList(allEmails);
    } catch (e) {
        document.getElementById('emailList').innerHTML =
            `<div class="empty-state"><i class="fas fa-exclamation-circle"></i><p>${e}</p></div>`;
    }
}

const _addrInfoCache = new Map();
function getAddrInfo(addr) {
    let info = _addrInfoCache.get(addr);
    if (!info) {
        info = { color: senderColor(addr), initials: senderInitials(addr), ico: getSenderIconUrl(addr) };
        _addrInfoCache.set(addr, info);
    }
    return info;
}

function emailItemHtml(email) {
    const avatarAddr = (currentFolder === 'Sent' || currentFolder === 'Drafts') ? email.to_addr : email.from_addr;
    const { color: bgColor, initials, ico } = getAddrInfo(avatarAddr);
    const avatarHtml = ico
        ? `<img class="ei-logo" src="${ico}" alt=""
               onerror="this.style.display='none';this.nextElementSibling.style.display='flex'"
               onload="this.nextElementSibling.style.display='none'">
           <div class="ei-avatar" style="background:${bgColor};display:none">${initials}</div>`
        : `<div class="ei-avatar" style="background:${bgColor}">${initials}</div>`;
    return `
    <div class="email-item ${email.is_read ? '' : 'unread'} ${email.id === currentEmailId ? 'active' : ''} ${selectedIds.has(email.id) ? 'selected' : ''}"
         id="ei-${email.id}" onclick="openEmail(${email.id})">
        <div class="ei-unread-dot" onclick="toggleEmailReadFromList(${email.id}, event)"
             title="${email.is_read ? 'Отметить как непрочитанное' : 'Отметить как прочитанное'}"></div>
        <div class="ei-avatar-zone">
            ${avatarHtml}
            <input type="checkbox" class="ei-check" ${selectedIds.has(email.id) ? 'checked' : ''}
                   onclick="toggleEmailCheck(${email.id}, event)" title="Выбрать">
        </div>
        <div class="ei-top">
            <span class="ei-from">${currentFolder === 'Drafts'
                ? (email.to_addr ? 'Кому: ' + escHtml(shortAddr(email.to_addr)) : '(без получателя)')
                : escHtml(shortAddr(email.from_addr))}</span>
            <span class="ei-date">${formatDate(email.date)}</span>
            <span class="ei-icons">
                ${email.is_starred ? '<i class="fas fa-star ei-star starred"></i>' : ''}
                ${email.has_attachment ? '<i class="fas fa-paperclip ei-attach"></i>' : ''}
            </span>
        </div>
        <div class="ei-subject">${escHtml(email.subject) || '(без темы)'}</div>
        <div class="ei-snippet">${escHtml(email.snippet)}</div>
    </div>`;
}

function renderEmailList(emails) {
    if (emails.length === 0) {
        document.getElementById('emailList').innerHTML =
            '<div class="empty-state"><i class="fas fa-inbox"></i><p>Писем нет</p></div>';
        return;
    }
    const loadMoreBtn = currentFolder !== 'Starred' ? `
        <div class="load-more-wrap" id="loadMoreWrap">
            <button class="load-more-btn" id="loadMoreBtn" onclick="loadMoreEmails()">
                <i class="fas fa-chevron-down"></i> Загрузить ещё
            </button>
        </div>` : '';
    document.getElementById('emailList').innerHTML =
        emails.map(emailItemHtml).join('') + loadMoreBtn;
}

// Добавляет новую страницу писем в конец списка без полного перерендера
function appendEmailsToList(emails) {
    const list = document.getElementById('emailList');
    // Удаляем кнопку "Загрузить ещё" если есть
    const wrap = document.getElementById('loadMoreWrap');
    if (wrap) wrap.remove();
    // Вставляем новые письма через DocumentFragment (один reflow)
    const frag = document.createDocumentFragment();
    const tmp = document.createElement('div');
    tmp.innerHTML = emails.map(emailItemHtml).join('');
    while (tmp.firstChild) frag.appendChild(tmp.firstChild);
    list.appendChild(frag);
    // Добавляем кнопку обратно
    if (currentFolder !== 'Starred') {
        const btnWrap = document.createElement('div');
        btnWrap.className = 'load-more-wrap';
        btnWrap.id = 'loadMoreWrap';
        btnWrap.innerHTML = `<button class="load-more-btn" id="loadMoreBtn" onclick="loadMoreEmails()">
            <i class="fas fa-chevron-down"></i> Загрузить ещё</button>`;
        list.appendChild(btnWrap);
    }
}

// ── Массовый выбор ─────────────────────────────────────────────────────────
function toggleEmailCheck(id, e) {
    e.stopPropagation();
    const el = document.getElementById('ei-' + id);
    if (selectedIds.has(id)) {
        selectedIds.delete(id);
        if (el) el.classList.remove('selected');
    } else {
        selectedIds.add(id);
        if (el) el.classList.add('selected');
    }
    // Обновляем состояние чекбокса в элементе
    if (el) {
        const cb = el.querySelector('.ei-check');
        if (cb) cb.checked = selectedIds.has(id);
    }
    updateSelectToolbar();
}

function toggleSelectAll(checked) {
    selectedIds.clear();
    if (checked) {
        allEmails.forEach(e => selectedIds.add(e.id));
    }
    // Обновляем классы и чекбоксы всех элементов
    document.querySelectorAll('.email-item').forEach(el => {
        const id = parseInt(el.id.replace('ei-', ''));
        if (!isNaN(id)) {
            el.classList.toggle('selected', selectedIds.has(id));
            const cb = el.querySelector('.ei-check');
            if (cb) cb.checked = selectedIds.has(id);
        }
    });
    updateSelectToolbar();
}

function clearSelection() {
    selectedIds.clear();
    document.querySelectorAll('.email-item.selected').forEach(el => {
        el.classList.remove('selected');
        const cb = el.querySelector('.ei-check');
        if (cb) cb.checked = false;
    });
    updateSelectToolbar();
}

function updateSelectToolbar() {
    const n = selectedIds.size;
    const toolbar = document.getElementById('selectToolbar');
    const countEl = document.getElementById('selectCount');
    const allCheck = document.getElementById('selectAllCheck');
    if (!toolbar) return;
    toolbar.classList.toggle('visible', n > 0);
    if (countEl) countEl.textContent = `${n} выбрано`;
    if (allCheck) {
        allCheck.checked = n > 0 && n >= allEmails.length;
        allCheck.indeterminate = n > 0 && n < allEmails.length;
    }
    const btn = document.getElementById('bulkActionBtn');
    if (btn) btn.title = currentFolder === 'Trash' ? 'Удалить навсегда' : 'В корзину';
}

async function bulkMarkRead(isRead) {
    if (selectedIds.size === 0) return;
    const ids = [...selectedIds];
    try {
        await invoke('mark_read_bulk', { emailIds: ids, isRead });
        ids.forEach(id => {
            const item = document.getElementById('ei-' + id);
            if (item) {
                item.classList.toggle('unread', !isRead);
                const dot = item.querySelector('.ei-unread-dot');
                if (dot) dot.title = isRead ? 'Отметить как непрочитанное' : 'Отметить как прочитанное';
            }
            const em = allEmails.find(x => x.id === id);
            if (em) em.is_read = isRead;
        });
        await updateUnreadBadge();
        clearSelection();
    } catch (e) { alert('Ошибка: ' + e); }
}

async function bulkDelete() {
    if (selectedIds.size === 0) return;
    const permanently = currentFolder === 'Trash';
    const label = permanently ? 'Удалить навсегда' : 'Переместить в корзину';
    if (!await window.__TAURI__.dialog.ask(`${label} ${selectedIds.size} писем?`, { title: 'Подтверждение', type: 'warning' })) return;

    const btn = document.getElementById('bulkActionBtn');
    if (btn) { btn.disabled = true; }

    try {
        await invoke('delete_emails_bulk', {
            emailIds: Array.from(selectedIds),
            permanently,
        });
        allEmails = allEmails.filter(e => !selectedIds.has(e.id));
        selectedIds.clear();
        updateSelectToolbar();
        renderEmailList(allEmails);
        document.getElementById('emailViewPanel').innerHTML =
            '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';
        await updateUnreadBadge();
    } catch (e) {
        alert('Ошибка: ' + e);
    } finally {
        if (btn) { btn.disabled = false; }
    }
}

function openMoveFolderPopup() {
    const popup = document.getElementById('moveFolderPopup');
    if (!popup) return;
    if (popup.classList.contains('open')) {
        popup.classList.remove('open');
        return;
    }
    // Системные папки (исключаем текущую, Черновики и Помечённые)
    const systemFolders = [
        { name: 'INBOX', label: 'Входящие',     icon: 'fa-inbox' },
        { name: 'Sent',  label: 'Отправленные', icon: 'fa-paper-plane' },
        { name: 'Spam',  label: 'Спам',          icon: 'fa-ban' },
        { name: 'Trash', label: 'Корзина',       icon: 'fa-trash' },
    ].filter(f => f.name !== currentFolder);

    // Пользовательские папки из сайдбара
    const customItems = Array.from(
        document.querySelectorAll('#customFoldersList .custom-folder-item')
    ).map(el => el.dataset.folder).filter(n => n && n !== currentFolder);

    let html = systemFolders.map(f =>
        `<div class="move-folder-item" onclick="moveSelectedToFolder('${escHtml(f.name)}')">
            <i class="fas ${f.icon}"></i> ${f.label}
        </div>`
    ).join('');
    if (customItems.length > 0) {
        html += '<div class="move-folder-sep"></div>';
        html += customItems.map(name =>
            `<div class="move-folder-item" onclick="moveSelectedToFolder('${escHtml(name)}')">
                <i class="fas fa-folder"></i> ${escHtml(name)}
            </div>`
        ).join('');
    }
    popup.innerHTML = html;
    popup.classList.add('open');
}

function showEmailImages() {
    const frame = document.getElementById('emailBodyFrame');
    if (frame) frame.contentWindow.postMessage({ type: 'showImages' }, '*');
    const banner = document.getElementById('imgBlockBanner');
    if (banner) banner.style.display = 'none';
}

function closeMovePopup() {
    const popup = document.getElementById('moveFolderPopup');
    if (popup) popup.classList.remove('open');
}

async function moveSelectedToFolder(targetFolder) {
    closeMovePopup();
    if (selectedIds.size === 0) return;
    const ids = [...selectedIds];
    try {
        await invoke('move_emails_to_folder', {
            accountId: currentAccountId,
            emailIds: ids,
            targetFolder,
        });
        allEmails = allEmails.filter(e => !selectedIds.has(e.id));
        selectedIds.clear();
        updateSelectToolbar();
        renderEmailList(allEmails);
        document.getElementById('emailViewPanel').innerHTML =
            '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';
    } catch (e) {
        alert('Ошибка перемещения: ' + e);
    }
}

// Закрываем popup при клике вне него
document.addEventListener('click', function(e) {
    const wrap = document.getElementById('moveFolderWrap');
    if (wrap && !wrap.contains(e.target)) closeMovePopup();
});

const FOLDER_LABELS = { INBOX: 'Входящие', Sent: 'Отправленные', Trash: 'Корзина', Spam: 'Спам', Drafts: 'Черновики', Starred: 'Избранное' };

// Подсвечивает вхождения query в rawText (безопасно, без XSS)
function highlightText(rawText, query) {
    if (!rawText || !query) return escHtml(rawText || '');
    const q = query.toLowerCase();
    const lower = rawText.toLowerCase();
    const parts = [];
    let last = 0, idx;
    while ((idx = lower.indexOf(q, last)) !== -1) {
        parts.push(escHtml(rawText.slice(last, idx)));
        parts.push(`<mark class="search-hl">${escHtml(rawText.slice(idx, idx + q.length))}</mark>`);
        last = idx + q.length;
    }
    parts.push(escHtml(rawText.slice(last)));
    return parts.join('');
}
let _isSearchMode = false;
let _searchDebounce = null;

function filterEmails(query) {
    clearTimeout(_searchDebounce);
    const q = query.trim();
    if (!q) {
        _isSearchMode = false;
        document.getElementById('searchInput').classList.remove('search-active');
        renderEmailList(allEmails);
        return;
    }
    if (q.length < 2) return;
    _searchDebounce = setTimeout(() => _runSearch(q), 350);
}

async function _runSearch(query) {
    if (!currentAccountId) return;
    _isSearchMode = true;
    document.getElementById('searchInput').classList.add('search-active');
    const list = document.getElementById('emailList');
    list.innerHTML = '<div class="empty-state"><i class="fas fa-circle-notch fa-spin"></i><p>Поиск...</p></div>';
    try {
        const results = await invoke('search_emails', { accountId: currentAccountId, query, limit: 200 });
        _renderSearchResults(results, query);
    } catch (e) {
        list.innerHTML = `<div class="empty-state"><i class="fas fa-exclamation-circle"></i><p>Ошибка поиска: ${escHtml(String(e))}</p></div>`;
    }
}

function _renderSearchResults(emails, query) {
    const list = document.getElementById('emailList');
    if (emails.length === 0) {
        list.innerHTML = `<div class="empty-state"><i class="fas fa-search"></i><p>Ничего не найдено по «${escHtml(query)}»</p></div>`;
        return;
    }
    list.innerHTML = emails.map(email => {
        const isSentOrDraft = email.folder === 'Sent' || email.folder === 'Drafts';
        const avatarAddr = isSentOrDraft ? email.to_addr : email.from_addr;
        const { color: bgColor, initials, ico } = getAddrInfo(avatarAddr);
        const avatarHtml = ico
            ? `<img class="ei-logo" src="${ico}" alt=""
                   onerror="this.style.display='none';this.nextElementSibling.style.display='flex'"
                   onload="this.nextElementSibling.style.display='none'">
               <div class="ei-avatar" style="background:${bgColor};display:none">${initials}</div>`
            : `<div class="ei-avatar" style="background:${bgColor}">${initials}</div>`;
        const folderLabel = FOLDER_LABELS[email.folder] || email.folder;
        return `
    <div class="email-item ${email.is_read ? '' : 'unread'} ${email.id === currentEmailId ? 'active' : ''}"
         id="ei-${email.id}" onclick="openEmail(${email.id})">
        <div class="ei-unread-dot" onclick="toggleEmailReadFromList(${email.id}, event)"
             title="${email.is_read ? 'Отметить как непрочитанное' : 'Отметить как прочитанное'}"></div>
        <div class="ei-avatar-zone">${avatarHtml}</div>
        <div class="ei-top">
            <span class="ei-from">${escHtml(shortAddr(avatarAddr))}</span>
            <span class="ei-date">${formatDate(email.date)}</span>
            <span class="ei-icons">
                ${email.is_starred ? '<i class="fas fa-star ei-star starred"></i>' : ''}
                ${email.has_attachment ? '<i class="fas fa-paperclip ei-attach"></i>' : ''}
            </span>
        </div>
        <div class="ei-subject">
            ${highlightText(email.subject, query) || '(без темы)'}
            <span class="ei-folder-badge">${escHtml(folderLabel)}</span>
        </div>
        <div class="ei-snippet">${highlightText(email.snippet, query)}</div>
    </div>`;
    }).join('') + `<div class="search-count-hint">${emails.length >= 200 ? '200+' : emails.length} результатов</div>`;
}

// ── Синхронизация ──────────────────────────────────────────────────────────
function imapFolderName() {
    return currentFolder === 'Sent' ? 'Sent'
        : currentFolder === 'Spam' ? 'Spam'
        : currentFolder === 'Trash' ? 'Trash'
        : 'INBOX';
}

async function syncCurrentFolder() {
    if (!currentAccountId) return;
    // Черновики — только локальные, IMAP-синк не нужен
    if (currentFolder === 'Drafts') { await loadEmails(); return; }
    const icon = document.getElementById('syncIcon');
    icon.classList.add('fa-spin');
    syncedCount = 0;
    _syncing = true;
    _syncNewCount = 0;
    sbStatus('syncing', 'Синхронизация...');
    const t0 = Date.now();
    try {
        // Rust возвращает [] при первичной загрузке; при инкременте — массив NotifItem.
        // Пока идёт ожидание — события email-received уже добавляют письма по одному.
        const newItems = await invoke('sync_folder', { accountId: currentAccountId, folder: imapFolderName(), offset: 0 });
        _syncing = false;
        // Перерисовываем список финально (правильный порядок, актуальные данные)
        await loadEmails();
        await updateUnreadBadge();
        const ms = Date.now() - t0;
        sbStatus('ok', `Готово (${(ms/1000).toFixed(1)}с)`, newItems.length);
        if (newItems && newItems.length > 0) {
            const saveBase = localStorage.getItem('mail-attach-path') || '';
            invoke('auto_save_attachments', { emailIds: newItems.map(n => n.id), saveBase }).catch(() => {});
            if (imapFolderName() === 'INBOX' && localStorage.getItem('mail-notifications') !== 'false') {
                const dur = parseInt(localStorage.getItem('mail-notif-duration') || '5');
                playMailSound();
                for (const item of newItems.slice(0, 3)) {
                    invoke('show_mail_notification', {
                        fromName: shortAddr(item.from_addr),
                        subject: item.subject || '(без темы)',
                        emailId: item.id,
                        durationSecs: dur,
                    }).catch(() => {});
                }
            }
        }
    } catch (e) {
        _syncing = false;
        sbStatus('error', 'Ошибка: ' + e);
    } finally {
        icon.classList.remove('fa-spin');
    }
}

async function loadMoreEmails() {
    if (!currentAccountId) return;
    const btn = document.getElementById('loadMoreBtn');
    if (btn) { btn.disabled = true; btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Загрузка...'; }
    try {
        // Сначала пробуем следующую страницу из БД (мгновенно)
        if (currentFolder !== 'Starred') {
            const morePage = await invoke('get_emails', {
                accountId: currentAccountId,
                folder: currentFolder,
                limit: PAGE_SIZE,
                offset: dbOffset,
            });
            if (morePage.length > 0) {
                // Добавляем к списку без полного перерендера
                allEmails = [...allEmails, ...morePage];
                dbOffset += morePage.length;
                appendEmailsToList(morePage);
                await updateUnreadBadge();
                if (btn) { btn.disabled = false; btn.innerHTML = '<i class="fas fa-chevron-down"></i> Загрузить ещё'; }
                return;
            }
        }
        // БД исчерпана — черновики не имеют IMAP-источника
        if (currentFolder === 'Drafts') {
            if (btn) { btn.disabled = false; btn.innerHTML = '<i class="fas fa-chevron-down"></i> Загрузить ещё'; }
            return;
        }
        // Остальные папки — идём на IMAP за более старыми письмами
        const newItems = await invoke('sync_folder', {
            accountId: currentAccountId,
            folder: imapFolderName(),
            offset: syncedCount,
        });
        syncedCount += newItems.length;
        await loadEmails();
        await updateUnreadBadge();
        if (newItems.length > 0) {
            const saveBase = localStorage.getItem('mail-attach-path') || '';
            invoke('auto_save_attachments', { emailIds: newItems.map(n => n.id), saveBase }).catch(() => {});
        }
    } catch (e) {
        alert('Ошибка загрузки: ' + e);
    } finally {
        if (btn) { btn.disabled = false; btn.innerHTML = '<i class="fas fa-chevron-down"></i> Загрузить ещё'; }
    }
}

async function updateUnreadBadge() {
    if (!currentAccountId) return;
    try {
        const count = await invoke('get_unread_count', { accountId: currentAccountId });
        // бейдж папки INBOX
        const badge = document.getElementById('badge-INBOX');
        if (count > 0) {
            badge.textContent = count;
            badge.style.display = 'inline';
        } else {
            badge.style.display = 'none';
        }
        // бейдж текущего аккаунта в сайдбаре — точечно, без полного ре-рендера
        accountUnreadCounts[currentAccountId] = count;
        updateAccountBadgeInDOM(currentAccountId, count);
    } catch {}
    updateDraftBadge();
}

// Обновляет только бейдж конкретного аккаунта в DOM (без перерисовки всего списка)
function updateAccountBadgeInDOM(accountId, count) {
    const items = document.querySelectorAll('.account-item');
    for (const item of items) {
        if (item.getAttribute('onclick') !== `selectAccount(${accountId})`) continue;
        let badge = item.querySelector('.account-unread');
        if (count > 0) {
            if (!badge) {
                badge = document.createElement('span');
                badge.className = 'account-unread';
                // вставляем перед кнопкой удаления
                const del = item.querySelector('.account-del');
                item.insertBefore(badge, del);
            }
            badge.textContent = count > 999 ? '999+' : count;
        } else {
            if (badge) badge.remove();
        }
        break;
    }
}

// Обновляет бейджи всех аккаунтов (вызывается после фонового синка)
async function refreshAllAccountBadges() {
    try {
        const list = await invoke('get_all_unread_counts');
        accountUnreadCounts = {};
        list.forEach(x => { accountUnreadCounts[x.account_id] = x.count; });
        // Обновляем каждый бейдж точечно
        for (const a of accounts) {
            updateAccountBadgeInDOM(a.id, accountUnreadCounts[a.id] || 0);
        }
    } catch {}
}

// ── Просмотр письма ────────────────────────────────────────────────────────
async function openEmail(id) {
    // Черновик — открываем в compose для редактирования
    if (currentFolder === 'Drafts') {
        openDraftInCompose(id);
        return;
    }

    currentEmailId = id;

    // Подсветка в списке
    document.querySelectorAll('.email-item').forEach(el => el.classList.remove('active'));
    const item = document.getElementById('ei-' + id);
    if (item) { item.classList.add('active'); item.classList.remove('unread'); }

    const panel = document.getElementById('emailViewPanel');
    panel.innerHTML = '<div class="empty-state"><i class="fas fa-spinner fa-spin"></i></div>';

    try {
        const email = await invoke('get_email_body', { emailId: id });
        await invoke('mark_read', { emailId: id, isRead: true });
        await updateUnreadBadge();

        // Блокируем внешние картинки (трекеры) как Outlook
        let hasExternalImages = false;
        let processedHtml = '';
        if (email.body_html) {
            const sanitized = sanitizeHtml(email.body_html);
            const doc = new DOMParser().parseFromString(sanitized, 'text/html');
            const BLANK = 'data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7';
            doc.querySelectorAll('img[src]').forEach(img => {
                const src = img.getAttribute('src');
                if (src && !src.startsWith('data:') && !src.startsWith('cid:')) {
                    img.setAttribute('data-src', src);
                    img.setAttribute('src', BLANK);
                    img.style.maxWidth = '100%';
                    hasExternalImages = true;
                }
            });
            // Добавляем отступы внутри письма
            let styleEl = doc.querySelector('style#docvis-padding');
            if (!styleEl) {
                styleEl = doc.createElement('style');
                styleEl.id = 'docvis-padding';
                (doc.head || doc.documentElement).appendChild(styleEl);
            }
            styleEl.textContent = 'body{padding:16px 36px 24px 36px!important;box-sizing:border-box;}';
            processedHtml = doc.documentElement.outerHTML;
        }

        const bodyContent = email.body_html
            ? `<iframe class="email-body-frame" id="emailBodyFrame" sandbox="allow-scripts allow-popups allow-popups-to-escape-sandbox"></iframe>`
            : `<pre class="email-body-text">${escHtml(email.body_text)}</pre>`;

        const isStarred = email.is_starred;
        const inTrash = currentFolder === 'Trash';
        const inSpam  = currentFolder === 'Spam';

        // Загружаем вложения
        const attachments = email.has_attachment
            ? await invoke('get_attachments', { emailId: id }).catch(() => [])
            : [];

        const attachHtml = attachments.length > 0 ? `
            <div class="attachments-panel">
                <div class="attachments-title">
                    <i class="fas fa-paperclip"></i> Вложения (${attachments.length})
                </div>
                <div class="attachments-list">
                    ${attachments.map(a => `
                        <div class="attach-item" data-path="${escHtml(a.file_path)}" onclick="openAttachment(this.dataset.path)">
                            ${attachIconHtml(a.filename, a.mime_type)}
                            <div class="attach-info">
                                <span class="attach-name">${escHtml(a.filename)}</span>
                                <span class="attach-size">${formatSize(a.file_size)}</span>
                            </div>
                            <i class="fas fa-external-link-alt attach-open"></i>
                        </div>
                    `).join('')}
                </div>
            </div>
        ` : '';

        panel.innerHTML = `
            <div class="email-view-header">
                <div class="ev-subject">${escHtml(email.subject) || '(без темы)'}</div>
                <div class="ev-meta">
                    <span><b>От:</b> ${escHtml(email.from_addr)}</span>
                    <span><b>Кому:</b> ${escHtml(email.to_addr)}</span>
                    ${email.cc_addr ? `<span><b>Копия:</b> ${escHtml(email.cc_addr)}</span>` : ''}
                    <span><b>Дата:</b> ${escHtml(email.date)}</span>
                </div>
                <div class="ev-actions">
                    <button class="ev-btn" onclick="replyTo(${id})">
                        <i class="fas fa-reply"></i> Ответить
                    </button>
                    <button class="ev-btn" onclick="forwardEmail(${id})">
                        <i class="fas fa-share"></i> Переслать
                    </button>
                    <button class="ev-btn" id="markReadBtn-${id}" data-read="1"
                            onclick="toggleMarkRead(${id})" title="Отметить как непрочитанное">
                        <i class="fas fa-envelope"></i> Непрочитанное
                    </button>
                    <button class="ev-btn ${isStarred ? 'star-active' : ''}" id="starBtn-${id}"
                            onclick="toggleStar(${id})">
                        <i class="fas fa-star"></i> ${isStarred ? 'Снять метку' : 'Отметить'}
                    </button>
                    <button class="ev-btn" onclick="addContactFromEmail(${id})" title="Сохранить отправителя в контакты">
                        <i class="fas fa-user-plus"></i>
                    </button>
                    ${inSpam
                        ? `<button class="ev-btn" onclick="moveFromSpam(${id})">
                               <i class="fas fa-inbox"></i> Не спам
                           </button>`
                        : `<button class="ev-btn warning" onclick="moveToSpam(${id})">
                               <i class="fas fa-ban"></i> В спам
                           </button>`
                    }
                    ${inTrash
                        ? `<button class="ev-btn danger" onclick="deletePermanently(${id})">
                               <i class="fas fa-trash"></i> Удалить навсегда
                           </button>`
                        : `<button class="ev-btn danger" onclick="moveToTrash(${id})">
                               <i class="fas fa-trash"></i> В корзину
                           </button>`
                    }
                </div>
            </div>
            ${hasExternalImages ? `
            <div class="img-block-banner" id="imgBlockBanner">
                <i class="fas fa-eye-slash"></i>
                <span>Внешние изображения скрыты</span>
                <button class="ev-btn" onclick="showEmailImages()">
                    <i class="fas fa-image"></i> Показать
                </button>
            </div>` : ''}
            ${(email.read_receipt_to && !email.read_receipt_sent) ? `
            <div class="mdn-banner" id="mdnBanner">
                <i class="fas fa-eye"></i>
                <span>Отправитель запрашивает уведомление о прочтении</span>
                <button class="ev-btn mdn-send-btn" onclick="sendReadReceipt(${id}, '${escHtml(email.read_receipt_to)}', '${escHtml(email.subject).replace(/'/g, "\\'")}')">
                    <i class="fas fa-check"></i> Отправить
                </button>
                <button class="icon-btn mdn-dismiss-btn" onclick="dismissReadReceipt(${id})" title="Не отправлять">
                    <i class="fas fa-times"></i>
                </button>
            </div>` : ''}
            ${attachHtml}
            <div class="email-view-body">${bodyContent}</div>
        `;
        // Изолируем HTML письма в iframe (стили письма не влияют на интерфейс)
        if (email.body_html) {
            const frame = document.getElementById('emailBodyFrame');
            if (frame) {
                const linkInterceptor = `<script>document.addEventListener('click',function(e){var a=e.target.closest('a');if(!a||!a.href||a.href.startsWith('mailto:'))return;e.preventDefault();window.parent.postMessage({type:'mailLink',url:a.href},'*');});<\/script>`;
                const imageRestorer = `<script>window.addEventListener('message',function(e){if(e.data&&e.data.type==='showImages'){document.querySelectorAll('img[data-src]').forEach(function(img){img.src=img.getAttribute('data-src');});}});<\/script>`;
                frame.srcdoc = processedHtml + linkInterceptor + imageRestorer;
            }
        }
    } catch (e) {
        panel.innerHTML = `<div class="empty-state"><i class="fas fa-exclamation-circle"></i><p>${e}</p></div>`;
    }
}

async function toggleEmailReadFromList(id, e) {
    e.stopPropagation();
    const item = document.getElementById('ei-' + id);
    if (!item) return;
    const isUnread = item.classList.contains('unread');
    const newRead = isUnread; // было непрочитанным → ставим прочитанным, и наоборот
    try {
        await invoke('mark_read', { emailId: id, isRead: newRead });
        item.classList.toggle('unread', !newRead);
        const dot = item.querySelector('.ei-unread-dot');
        if (dot) dot.title = newRead ? 'Отметить как непрочитанное' : 'Отметить как прочитанное';
        // Синхронизируем кнопку в просмотре (если письмо открыто)
        const btn = document.getElementById('markReadBtn-' + id);
        if (btn) {
            btn.dataset.read = newRead ? '1' : '0';
            btn.innerHTML = newRead
                ? '<i class="fas fa-envelope"></i> Непрочитанное'
                : '<i class="fas fa-envelope-open"></i> Прочитанное';
        }
        const em = allEmails.find(x => x.id === id);
        if (em) em.is_read = newRead;
        await updateUnreadBadge();
    } catch {}
}

async function toggleMarkRead(id) {
    const btn = document.getElementById('markReadBtn-' + id);
    if (!btn) return;
    const nowRead = btn.dataset.read === '1';
    const newRead = !nowRead;
    try {
        await invoke('mark_read', { emailId: id, isRead: newRead });
        // Обновляем кнопку
        btn.dataset.read = newRead ? '1' : '0';
        if (newRead) {
            btn.innerHTML = '<i class="fas fa-envelope"></i> Непрочитанное';
            btn.title = 'Отметить как непрочитанное';
        } else {
            btn.innerHTML = '<i class="fas fa-envelope-open"></i> Прочитанное';
            btn.title = 'Отметить как прочитанное';
        }
        // Обновляем иконку в списке писем
        const item = document.getElementById('ei-' + id);
        if (item) item.classList.toggle('unread', !newRead);
        // Обновляем локальный массив и бейдж
        const em = allEmails.find(e => e.id === id);
        if (em) em.is_read = newRead;
        await updateUnreadBadge();
    } catch (e) { console.error(e); }
}

async function toggleStar(id) {
    const starred = await invoke('toggle_starred', { emailId: id });
    const btn = document.getElementById('starBtn-' + id);
    if (btn) {
        btn.className = `ev-btn ${starred ? 'star-active' : ''}`;
        btn.innerHTML = `<i class="fas fa-star"></i> ${starred ? 'Снять метку' : 'Отметить'}`;
    }
    // Обновить иконку в списке
    const ei = document.getElementById('ei-' + id);
    if (ei) {
        const icons = ei.querySelector('.ei-icons');
        if (icons) {
            const starEl = icons.querySelector('.ei-star');
            if (starred && !starEl) {
                icons.insertAdjacentHTML('afterbegin', '<i class="fas fa-star ei-star starred"></i>');
            } else if (!starred && starEl) {
                starEl.remove();
            }
        }
    }
}

async function clearTrash() {
    if (!currentAccountId) return;
    if (allEmails.length === 0) return;
    if (!await window.__TAURI__.dialog.ask(
        `Удалить навсегда все ${allEmails.length} писем из корзины?`,
        { title: 'Очистить корзину', type: 'warning' }
    )) return;

    const btn = document.getElementById('clearTrashBtn');
    if (btn) btn.disabled = true;
    try {
        await invoke('clear_trash', { accountId: currentAccountId });
        allEmails = [];
        selectedIds.clear();
        updateSelectToolbar();
        renderEmailList(allEmails);
        document.getElementById('emailViewPanel').innerHTML =
            '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';
    } catch (e) {
        alert('Ошибка: ' + e);
    } finally {
        if (btn) btn.disabled = false;
    }
}

async function moveToSpam(id) {
    await invoke('move_to_spam', { emailId: id });
    await invoke('block_sender', { emailId: id });
    allEmails = allEmails.filter(e => e.id !== id);
    renderEmailList(allEmails);
    document.getElementById('emailViewPanel').innerHTML =
        '<div class="empty-state large"><i class="fas fa-ban"></i><p>Письмо в спаме, отправитель заблокирован</p></div>';
}

async function moveFromSpam(id) {
    await invoke('move_from_spam', { emailId: id });
    await invoke('unblock_sender', { emailId: id });
    allEmails = allEmails.filter(e => e.id !== id);
    renderEmailList(allEmails);
    document.getElementById('emailViewPanel').innerHTML =
        '<div class="empty-state large"><i class="fas fa-inbox"></i><p>Письмо во Входящих, отправитель разблокирован</p></div>';
}

async function moveToTrash(id) {
    if (!await window.__TAURI__.dialog.ask('Переместить письмо в корзину?', { title: 'Подтверждение', type: 'warning' })) return;
    await invoke('move_to_trash', { emailId: id });
    allEmails = allEmails.filter(e => e.id !== id);
    renderEmailList(allEmails);
    document.getElementById('emailViewPanel').innerHTML =
        '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';
}

async function deletePermanently(id) {
    if (!await window.__TAURI__.dialog.ask('Удалить письмо навсегда?', { title: 'Подтверждение', type: 'warning' })) return;
    await invoke('delete_permanently', { emailId: id });
    allEmails = allEmails.filter(e => e.id !== id);
    renderEmailList(allEmails);
    document.getElementById('emailViewPanel').innerHTML =
        '<div class="empty-state large"><i class="fas fa-envelope-open-text"></i><p>Выберите письмо</p></div>';
}

// ── Ответить ───────────────────────────────────────────────────────────────
async function replyTo(id) {
    try {
        const email = await invoke('get_email_body', { emailId: id });
        openCompose('Ответить');
        const fromMatch = email.from_addr.match(/<(.+?)>/) || [null, email.from_addr];
        document.getElementById('composeTo').value = fromMatch[1];
        document.getElementById('composeSubject').value =
            email.subject.startsWith('Re:') ? email.subject : 'Re: ' + email.subject;

        const origHtml = email.body_html
            || `<pre style="white-space:pre-wrap;margin:0">${escHtml(email.body_text)}</pre>`;
        const body = document.getElementById('composeBody');
        body.innerHTML =
            `<p><br></p>` +
            `<blockquote>` +
            `<div class="compose-quote-header">${escHtml(email.from_addr)} написал(а):</div>` +
            origHtml +
            `</blockquote>`;
        composeMoveCursorToStart(body);
    } catch {}
}

// ── Переслать ───────────────────────────────────────────────────────────────
let _forwardHtml = null; // не используется, оставлен для openCompose reset

async function forwardEmail(id) {
    try {
        const [email, attachments] = await Promise.all([
            invoke('get_email_body', { emailId: id }),
            invoke('get_attachments', { emailId: id }).catch(() => []),
        ]);

        openCompose('Переслать');
        document.getElementById('composeTo').value = '';
        document.getElementById('composeSubject').value =
            email.subject.startsWith('Fwd:') ? email.subject : 'Fwd: ' + email.subject;

        const metaHtml = [
            `<b>От:</b> ${escHtml(email.from_addr)}`,
            `<b>Кому:</b> ${escHtml(email.to_addr)}`,
            `<b>Дата:</b> ${escHtml(email.date)}`,
            `<b>Тема:</b> ${escHtml(email.subject)}`,
        ].join('<br>');
        const origHtml = email.body_html
            || `<pre style="white-space:pre-wrap;margin:0">${escHtml(email.body_text)}</pre>`;

        const body = document.getElementById('composeBody');
        body.innerHTML =
            `<p><br></p>` +
            `<div style="border-top:1px solid var(--border);padding-top:10px;margin-top:4px">` +
            `<div class="compose-quote-header">-------- Пересланное сообщение --------<br>${metaHtml}</div>` +
            `<div>${origHtml}</div>` +
            `</div>`;
        composeMoveCursorToStart(body);

        // Прикрепляем оригинальные вложения
        for (const att of attachments) {
            try {
                const res = await invoke('read_attachment_b64', { filePath: att.file_path });
                const bytes = Uint8Array.from(atob(res.b64), c => c.charCodeAt(0));
                const file = new File([bytes], att.filename, { type: res.mime_type || att.mime_type });
                composeFiles.push(file);
            } catch {}
        }
        renderComposeAttachList();
        setTimeout(() => document.getElementById('composeTo').focus(), 50);
    } catch(e) { alert('Ошибка: ' + e); }
}

// ── Compose ────────────────────────────────────────────────────────────────

// Перемещает курсор в начало contenteditable div (перед цитатой)
function composeMoveCursorToStart(el) {
    try {
        const range = document.createRange();
        range.setStart(el.firstChild || el, 0);
        range.collapse(true);
        const sel = window.getSelection();
        sel.removeAllRanges();
        sel.addRange(range);
        el.focus();
    } catch {}
}

// ── Автодополнение адресатов ────────────────────────────────────────────────
let _addrDdIdx = -1;

function onAddrInput(inputId, dropdownId, value) {
    const term = value.trim().toLowerCase();
    const dd = document.getElementById(dropdownId);
    if (!term || contacts.length === 0) { dd.style.display = 'none'; return; }

    const matched = contacts.filter(c =>
        c.name.toLowerCase().includes(term) || c.email.toLowerCase().includes(term)
    ).slice(0, 8);

    if (matched.length === 0) { dd.style.display = 'none'; return; }

    _addrDdIdx = -1;
    dd.innerHTML = matched.map((c, i) => {
        const display = c.name ? `${c.name} <${c.email}>` : c.email;
        return `<div class="cd-item" data-idx="${i}" data-value="${escHtml(display)}"
                     data-input="${inputId}" data-dd="${dropdownId}"
                     onclick="selectAddrContact(this)">
            <div class="cd-avatar" style="background:${senderColor(c.email)}">${senderInitials(display)}</div>
            <div>
                <div class="cd-name">${escHtml(c.name || c.email)}</div>
                ${c.name ? `<div class="cd-email">${escHtml(c.email)}</div>` : ''}
            </div>
        </div>`;
    }).join('');
    dd.style.display = 'block';
}

function selectAddrContact(el) {
    document.getElementById(el.dataset.input).value = el.dataset.value;
    document.getElementById(el.dataset.dd).style.display = 'none';
}

function onAddrKeydown(e, dropdownId) {
    const dd = document.getElementById(dropdownId);
    if (dd.style.display === 'none') return;
    const items = [...dd.querySelectorAll('.cd-item')];
    if (!items.length) return;
    if (e.key === 'ArrowDown') {
        e.preventDefault();
        _addrDdIdx = Math.min(_addrDdIdx + 1, items.length - 1);
    } else if (e.key === 'ArrowUp') {
        e.preventDefault();
        _addrDdIdx = Math.max(_addrDdIdx - 1, 0);
    } else if (e.key === 'Enter' && _addrDdIdx >= 0) {
        e.preventDefault();
        items[_addrDdIdx]?.click();
        return;
    } else if (e.key === 'Escape') {
        dd.style.display = 'none'; return;
    } else { return; }
    items.forEach((el, i) => el.classList.toggle('cd-active', i === _addrDdIdx));
    items[_addrDdIdx]?.scrollIntoView({ block: 'nearest' });
}

function closeAddrDropdowns() {
    document.querySelectorAll('.contacts-dropdown').forEach(d => d.style.display = 'none');
}

// ── Диалог выбора адресата ──────────────────────────────────────────────────
let _addrPickerTarget = null;
let _pickerSelected = new Map(); // email → display-строка

function openAddrPicker(inputId) {
    _addrPickerTarget = inputId;
    _pickerSelected = new Map();
    document.getElementById('addrPickerSearch').value = '';
    document.getElementById('addrPickerModal').style.display = 'flex';
    if (!contactsLoaded) {
        invoke('get_contacts').then(list => {
            contacts = list; contactsLoaded = true; renderPickerList('');
        }).catch(() => {});
    }
    renderPickerList('');
    updatePickerFooter();
    setTimeout(() => document.getElementById('addrPickerSearch').focus(), 50);
}

function closeAddrPicker() {
    document.getElementById('addrPickerModal').style.display = 'none';
}

function closeAddrPickerOverlay(e) {
    if (e.target === document.getElementById('addrPickerModal')) closeAddrPicker();
}

function updatePickerFooter() {
    const n = _pickerSelected.size;
    document.getElementById('pickerCount').textContent =
        n === 0 ? 'Ничего не выбрано' : `Выбрано: ${n}`;
    document.getElementById('pickerAddBtn').disabled = n === 0;
}

function togglePickerItem(row) {
    const email = row.dataset.email;
    const display = row.dataset.display;
    if (_pickerSelected.has(email)) {
        _pickerSelected.delete(email);
        row.classList.remove('picker-checked');
        row.querySelector('.picker-check').checked = false;
    } else {
        _pickerSelected.set(email, display);
        row.classList.add('picker-checked');
        row.querySelector('.picker-check').checked = true;
    }
    updatePickerFooter();
}

function confirmPickerSelection() {
    if (!_addrPickerTarget || _pickerSelected.size === 0) return;
    const input = document.getElementById(_addrPickerTarget);
    // Добавляем к уже введённым адресам (если есть)
    const existing = input.value.trim();
    const newAddrs = [..._pickerSelected.values()].join(', ');
    input.value = existing ? existing + ', ' + newAddrs : newAddrs;
    closeAddrPicker();
}

function renderPickerList(term) {
    const q = term.trim().toLowerCase();
    const list = q
        ? contacts.filter(c => c.name.toLowerCase().includes(q) || c.email.toLowerCase().includes(q))
        : contacts;

    const el = document.getElementById('addrPickerList');
    if (list.length === 0) {
        el.innerHTML = `<div class="addr-picker-empty">
            <i class="fas fa-${q ? 'search' : 'users'}"></i>
            <p>${q ? 'Ничего не найдено' : 'Нет контактов'}</p>
        </div>`;
        return;
    }

    const groups = {};
    for (const c of list) {
        const key = (c.name || c.email || '?')[0].toUpperCase();
        const letter = /[А-ЯЁA-Z]/.test(key) ? key : '#';
        if (!groups[letter]) groups[letter] = [];
        groups[letter].push(c);
    }
    const letters = Object.keys(groups).sort((a, b) => {
        if (a === '#') return 1; if (b === '#') return -1;
        return a.localeCompare(b, 'ru');
    });

    el.innerHTML = letters.map(letter => `
        <div class="contact-group-letter">${letter}</div>
        ${groups[letter].map(c => {
            const display = c.name ? `${c.name} <${c.email}>` : c.email;
            const checked = _pickerSelected.has(c.email);
            return `<div class="addr-picker-item ${checked ? 'picker-checked' : ''}"
                         data-email="${escHtml(c.email)}" data-display="${escHtml(display)}"
                         onclick="togglePickerItem(this)">
                <input type="checkbox" class="picker-check" ${checked ? 'checked' : ''}
                       onclick="event.stopPropagation(); togglePickerItem(this.closest('.addr-picker-item'))">
                <div class="cd-avatar" style="background:${senderColor(c.email)}">${senderInitials(display)}</div>
                <div>
                    <div class="cd-name">${escHtml(c.name || c.email)}</div>
                    ${c.name ? `<div class="cd-email">${escHtml(c.email)}</div>` : ''}
                </div>
            </div>`;
        }).join('')}
    `).join('');
}

function openCompose(title = 'Новое письмо') {
    currentDraftId = null;
    document.getElementById('composeTo').value = '';
    document.getElementById('composeCc').value = '';
    document.getElementById('composeSubject').value = '';
    // Вставляем подпись если задана
    const sig = localStorage.getItem('mail-signature') || '';
    document.getElementById('composeBody').innerHTML =
        sig ? `<p><br></p><p>--<br>${sig.replace(/\n/g, '<br>')}</p>` : '';
    document.getElementById('composeError').style.display = 'none';
    document.getElementById('draftSavedHint').style.display = 'none';
    composeFiles = [];
    _forwardHtml = null;
    renderComposeAttachList();
    document.getElementById('composeTitleText').textContent = title;
    document.getElementById('composeModal').style.display = 'flex';
    closeAddrDropdowns();
    if (!contactsLoaded) invoke('get_contacts').then(list => { contacts = list; contactsLoaded = true; }).catch(() => {});
    setTimeout(() => document.getElementById('composeTo').focus(), 50);
    startDraftAutoSave();
}

async function openDraftInCompose(draftId) {
    try {
        const email = await invoke('get_email_body', { emailId: draftId });
        currentDraftId = draftId;
        document.getElementById('composeTo').value = email.to_addr || '';
        document.getElementById('composeCc').value = email.cc_addr || '';
        document.getElementById('composeSubject').value = email.subject || '';
        const body = document.getElementById('composeBody');
        body.innerHTML = email.body_html || email.body_text.replace(/\n/g, '<br>') || '';
        document.getElementById('composeError').style.display = 'none';
        document.getElementById('draftSavedHint').style.display = 'none';
        composeFiles = [];
        _forwardHtml = null;
        renderComposeAttachList();
        document.getElementById('composeTitleText').textContent = 'Черновик';
        document.getElementById('composeModal').style.display = 'flex';
        closeAddrDropdowns();
        if (!contactsLoaded) invoke('get_contacts').then(list => { contacts = list; contactsLoaded = true; }).catch(() => {});
        setTimeout(() => body.focus(), 50);
        startDraftAutoSave();
    } catch (e) {
        alert('Не удалось открыть черновик: ' + e);
    }
}

function closeCompose() {
    stopDraftAutoSave();
    composeFiles = [];
    currentDraftId = null;
    _dragCounter = 0;
    document.querySelector('.compose-attach-zone')?.classList.remove('drag-over');
    document.getElementById('composeModal').style.display = 'none';
}

// ── Черновики ────────────────────────────────────────────────────────────────
async function saveDraft(manual = false) {
    if (!currentAccountId) return;
    const to       = document.getElementById('composeTo').value.trim();
    const cc       = document.getElementById('composeCc').value.trim();
    const subject  = document.getElementById('composeSubject').value.trim();
    const bodyEl   = document.getElementById('composeBody');
    const bodyText = bodyEl.innerText.trim();
    const bodyHtml = bodyEl.innerHTML;

    // Не сохраняем пустой черновик
    if (!to && !subject && !bodyText) return;

    try {
        const newId = await invoke('save_draft', {
            accountId: currentAccountId,
            draftId: currentDraftId,
            to, cc, subject,
            bodyText, bodyHtml,
        });
        currentDraftId = newId;
        // Показываем индикатор "Сохранено" с анимацией
        // Сбрасываем анимацию если она уже шла (повторное сохранение)
        const hint = document.getElementById('draftSavedHint');
        hint.style.display = 'none';
        void hint.offsetWidth; // reflow — сбрасывает animation
        hint.style.display = 'inline-flex';
        // Обновляем бейдж папки черновиков
        updateDraftBadge();
        if (manual && currentFolder === 'Drafts') loadEmails();
    } catch (e) {
        if (manual) alert('Не удалось сохранить черновик: ' + e);
    }
}

function startDraftAutoSave() {
    stopDraftAutoSave();
    // Автосохранение каждые 30 секунд
    draftAutoSaveTimer = setInterval(() => saveDraft(false), 30000);
}

function stopDraftAutoSave() {
    if (draftAutoSaveTimer) { clearInterval(draftAutoSaveTimer); draftAutoSaveTimer = null; }
}

async function updateDraftBadge() {
    if (!currentAccountId) return;
    try {
        const count = await invoke('get_draft_count', { accountId: currentAccountId });
        const badge = document.getElementById('badge-Drafts');
        if (count > 0) {
            badge.textContent = count;
            badge.style.display = 'inline';
        } else {
            badge.style.display = 'none';
        }
    } catch {}
}

// ── Вложения в письме ────────────────────────────────────────────────────────
function handleComposeFiles(fileList) {
    for (const f of fileList) composeFiles.push(f);
    renderComposeAttachList();
}

function removeComposeFile(idx) {
    composeFiles.splice(idx, 1);
    renderComposeAttachList();
}

function renderComposeAttachList() {
    const el = document.getElementById('composeAttachList');
    if (!el) return;
    if (composeFiles.length === 0) { el.innerHTML = ''; return; }
    el.innerHTML = composeFiles.map((f, i) => `
        <div class="compose-attach-chip">
            ${attachIconHtml(f.name, f.type || '')}
            <span title="${escHtml(f.name)}">${escHtml(f.name)}</span>
            <span class="chip-size">${formatSize(f.size)}</span>
            <button onclick="removeComposeFile(${i})" title="Убрать"><i class="fas fa-times"></i></button>
        </div>
    `).join('');
}


async function sendReadReceipt(emailId, to, subject) {
    try {
        await invoke('send_mdn', {
            emailId, accountId: currentAccountId, to, originalSubject: subject
        });
        document.getElementById('mdnBanner')?.remove();
    } catch (e) {
        alert('Не удалось отправить уведомление: ' + e);
    }
}

async function dismissReadReceipt(emailId) {
    await invoke('dismiss_read_receipt', { emailId }).catch(() => {});
    document.getElementById('mdnBanner')?.remove();
}

function showSendToast(success, msg) {
    let toast = document.getElementById('sendResultToast');
    if (!toast) {
        toast = document.createElement('div');
        toast.id = 'sendResultToast';
        toast.className = 'attach-toast';
        document.body.appendChild(toast);
    }
    toast.className = 'attach-toast ' + (success ? 'success' : 'error');
    toast.innerHTML = success
        ? '<i class="fas fa-check-circle"></i> ' + (msg || 'Письмо отправлено')
        : '<i class="fas fa-exclamation-circle"></i> ' + (msg || 'Ошибка отправки');
    toast.classList.add('visible');
    clearTimeout(toast._timer);
    toast._timer = setTimeout(() => toast.classList.remove('visible'), success ? 3000 : 5000);
}

function fileToBase64(file) {
    return new Promise((resolve, reject) => {
        const reader = new FileReader();
        reader.onload = () => {
            const bytes = new Uint8Array(reader.result);
            let binary = '';
            for (let i = 0; i < bytes.byteLength; i++) binary += String.fromCharCode(bytes[i]);
            resolve(btoa(binary));
        };
        reader.onerror = reject;
        reader.readAsArrayBuffer(file);
    });
}

async function sendMail() {
    const to      = document.getElementById('composeTo').value.trim();
    const cc        = document.getElementById('composeCc').value.trim();
    const subject   = document.getElementById('composeSubject').value.trim();
    const bodyEl    = document.getElementById('composeBody');
    const body      = bodyEl.innerText.trim();   // plain text для SMTP text/plain
    const body_html = bodyEl.innerHTML;          // HTML с форматированием оригинала

    if (!to || !body) {
        showComposeError('Укажите получателя и текст письма');
        return;
    }
    if (!currentAccountId) {
        showComposeError('Аккаунт не выбран');
        return;
    }

    const btn = document.getElementById('sendBtn');
    btn.disabled = true;
    btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Отправка...';

    try {
        // Читаем файлы как base64
        const attachments = await Promise.all(composeFiles.map(async f => ({
            filename: f.name,
            data_b64: await fileToBase64(f),
            mime_type: f.type || 'application/octet-stream',
        })));

        const read_receipt = document.getElementById('composeReadReceipt')?.checked ?? false;
        await invoke('send_mail', {
            req: { account_id: currentAccountId, to, cc, subject, body, body_html, attachments, read_receipt }
        });
        // Удаляем черновик если письмо было создано из него
        if (currentDraftId) {
            await invoke('delete_draft', { draftId: currentDraftId }).catch(() => {});
            updateDraftBadge();
            if (currentFolder === 'Drafts') loadEmails();
        }
        closeCompose();
        if (currentFolder === 'Sent') loadEmails();
        showSendToast(true);
    } catch (e) {
        showSendToast(false, 'Ошибка: ' + e);
        showComposeError('Ошибка: ' + e);
    } finally {
        btn.disabled = false;
        btn.innerHTML = '<i class="fas fa-paper-plane"></i> Отправить';
    }
}

function showComposeError(msg) {
    const el = document.getElementById('composeError');
    el.textContent = msg;
    el.style.display = 'block';
}

// ── Утилиты ────────────────────────────────────────────────────────────────
async function openAttachment(filePath) {
    try {
        const saveBase = localStorage.getItem('mail-attach-path') || null;
        const subfolder = currentFolder === 'Sent' ? 'Отправленные' : null;
        const savedTo = await invoke('open_attachment', { filePath, saveBase, subfolder });
        showSavedToast(savedTo);
    } catch (e) {
        alert('Не удалось открыть файл: ' + e);
    }
}

function showSavedToast(path) {
    // Показываем уведомление куда сохранился файл
    let toast = document.getElementById('attachToast');
    if (!toast) {
        toast = document.createElement('div');
        toast.id = 'attachToast';
        toast.className = 'attach-toast';
        document.body.appendChild(toast);
    }
    // Берём только имя папки (последние 3 части пути: дата/время)
    const parts = path.replace(/\\/g, '/').split('/');
    const short = parts.slice(-4).join('/');
    toast.textContent = 'Сохранено: Почта/' + short;
    toast.classList.add('visible');
    clearTimeout(toast._timer);
    toast._timer = setTimeout(() => toast.classList.remove('visible'), 3000);
}

const _EXT_ICON = {
    // PDF
    pdf:  { icon: 'fas fa-file-pdf',     color: '#e53935' },
    // Word
    doc:  { icon: 'fas fa-file-word',    color: '#1565c0' },
    docx: { icon: 'fas fa-file-word',    color: '#1565c0' },
    odt:  { icon: 'fas fa-file-word',    color: '#1565c0' },
    rtf:  { icon: 'fas fa-file-word',    color: '#1565c0' },
    // Excel
    xls:  { icon: 'fas fa-file-excel',   color: '#2e7d32' },
    xlsx: { icon: 'fas fa-file-excel',   color: '#2e7d32' },
    ods:  { icon: 'fas fa-file-excel',   color: '#2e7d32' },
    csv:  { icon: 'fas fa-file-excel',   color: '#2e7d32' },
    // PowerPoint
    ppt:  { icon: 'fas fa-file-powerpoint', color: '#e65100' },
    pptx: { icon: 'fas fa-file-powerpoint', color: '#e65100' },
    odp:  { icon: 'fas fa-file-powerpoint', color: '#e65100' },
    // Архивы
    zip:  { icon: 'fas fa-file-archive', color: '#f9a825' },
    rar:  { icon: 'fas fa-file-archive', color: '#f9a825' },
    '7z': { icon: 'fas fa-file-archive', color: '#f9a825' },
    gz:   { icon: 'fas fa-file-archive', color: '#f9a825' },
    tar:  { icon: 'fas fa-file-archive', color: '#f9a825' },
    // Изображения
    jpg:  { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    jpeg: { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    png:  { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    gif:  { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    webp: { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    svg:  { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    bmp:  { icon: 'fas fa-file-image',   color: '#6a1b9a' },
    // Видео
    mp4:  { icon: 'fas fa-file-video',   color: '#00838f' },
    avi:  { icon: 'fas fa-file-video',   color: '#00838f' },
    mov:  { icon: 'fas fa-file-video',   color: '#00838f' },
    mkv:  { icon: 'fas fa-file-video',   color: '#00838f' },
    // Аудио
    mp3:  { icon: 'fas fa-file-audio',   color: '#ad1457' },
    wav:  { icon: 'fas fa-file-audio',   color: '#ad1457' },
    ogg:  { icon: 'fas fa-file-audio',   color: '#ad1457' },
    // Текст/код
    txt:  { icon: 'fas fa-file-alt',     color: '#546e7a' },
    xml:  { icon: 'fas fa-file-code',    color: '#546e7a' },
    json: { icon: 'fas fa-file-code',    color: '#546e7a' },
    html: { icon: 'fas fa-file-code',    color: '#546e7a' },
    css:  { icon: 'fas fa-file-code',    color: '#546e7a' },
    js:   { icon: 'fas fa-file-code',    color: '#546e7a' },
    // Исполняемые
    exe:  { icon: 'fas fa-file',         color: '#78909c' },
    msi:  { icon: 'fas fa-file',         color: '#78909c' },
};

function attachIconHtml(filename, mimeType) {
    const ext = (filename.split('.').pop() || '').toLowerCase();
    const byExt = _EXT_ICON[ext];
    if (byExt) return `<i class="${byExt.icon} attach-icon" style="color:${byExt.color}"></i>`;

    // Запасной вариант — по MIME типу
    const m = mimeType.toLowerCase();
    if (m.includes('pdf'))         return `<i class="fas fa-file-pdf attach-icon" style="color:#e53935"></i>`;
    if (m.includes('word') || m.includes('msword'))
                                   return `<i class="fas fa-file-word attach-icon" style="color:#1565c0"></i>`;
    if (m.includes('excel') || m.includes('spreadsheet'))
                                   return `<i class="fas fa-file-excel attach-icon" style="color:#2e7d32"></i>`;
    if (m.includes('powerpoint') || m.includes('presentation'))
                                   return `<i class="fas fa-file-powerpoint attach-icon" style="color:#e65100"></i>`;
    if (m.includes('zip') || m.includes('rar') || m.includes('archive'))
                                   return `<i class="fas fa-file-archive attach-icon" style="color:#f9a825"></i>`;
    if (m.includes('image/'))      return `<i class="fas fa-file-image attach-icon" style="color:#6a1b9a"></i>`;
    if (m.includes('video/'))      return `<i class="fas fa-file-video attach-icon" style="color:#00838f"></i>`;
    if (m.includes('audio/'))      return `<i class="fas fa-file-audio attach-icon" style="color:#ad1457"></i>`;
    if (m.includes('text/'))       return `<i class="fas fa-file-alt attach-icon" style="color:#546e7a"></i>`;
    return `<i class="fas fa-file attach-icon" style="color:#78909c"></i>`;
}

function formatSize(bytes) {
    if (bytes < 1024) return bytes + ' Б';
    if (bytes < 1024 * 1024) return (bytes / 1024).toFixed(1) + ' КБ';
    return (bytes / (1024 * 1024)).toFixed(1) + ' МБ';
}

function escHtml(s) {
    if (!s) return '';
    return s.replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
}

function shortAddr(addr) {
    if (!addr) return '';
    const m = addr.match(/^(.+?)\s*</);
    return m ? m[1].trim() : addr;
}

// ── Логотипы известных сервисов ─────────────────────────────────────────────
// domain → имя файла в img/si/ (null = имя домена без TLD не подходит, указываем явно)
const KNOWN_SERVICE_DOMAINS = {
    // Соцсети и мессенджеры
    'vk.com': 'vk', 'vk.ru': 'vk',
    'ok.ru': 'ok', 'odnoklassniki.ru': 'ok',
    'telegram.org': 'telegram', 'tg.dev': 'telegram', 't.me': 'telegram',
    'facebook.com': 'facebook', 'meta.com': 'facebook',
    'instagram.com': 'instagram',
    'twitter.com': 'twitter', 'x.com': 'twitter',
    'youtube.com': 'youtube',
    'linkedin.com': 'linkedin',
    'tiktok.com': 'tiktok',
    // Почта и поисковики
    'yandex.ru': 'yandex', 'ya.ru': 'yandex', 'yandex.com': 'yandex',
    'mail.ru': 'mailru', 'inbox.ru': 'mailru', 'bk.ru': 'mailru', 'list.ru': 'mailru',
    'google.com': 'google', 'gmail.com': 'google',
    'outlook.com': 'outlook', 'hotmail.com': 'outlook', 'live.com': 'outlook',
    'yahoo.com': 'yahoo',
    // Госуслуги и госорганы
    'gosuslugi.ru': 'gosuslugi', 'esia.gosuslugi.ru': 'gosuslugi', 'no-reply.gosuslugi.ru': 'gosuslugi',
    'nalog.ru': 'nalog', 'nalog.gov.ru': 'nalog',
    'sfr.gov.ru': 'sfr', 'pfr.gov.ru': 'sfr',
    'mos.ru': 'mos',
    // Банки
    'sber.ru': 'sber', 'sberbank.ru': 'sber', 'sbrf.ru': 'sber', 'sberinsurance.ru': 'sber', 'sbermegamarket.ru': 'sber',
    'tinkoff.ru': 'tinkoff', 't-bank.ru': 'tinkoff',
    'vtb.ru': 'vtb',
    'alfabank.ru': 'alfabank', 'alfabank.com': 'alfabank',
    'gazprombank.ru': 'gazprombank',
    'raiffeisen.ru': 'raiffeisen',
    'rosbank.ru': 'rosbank',
    'pochtabank.ru': 'pochtabank',
    'otkritie.ru': 'otkritie',
    'sovcombank.ru': 'sovcombank',
    'rshb.ru': 'rshb',
    'bspb.ru': 'bspb',
    // Маркетплейсы и магазины
    'ozon.ru': 'ozon',
    'wildberries.ru': 'wildberries', 'wb.ru': 'wildberries',
    'avito.ru': 'avito',
    'aliexpress.com': 'aliexpress', 'aliexpress.ru': 'aliexpress',
    'amazon.com': 'amazon', 'amazon.ru': 'amazon',
    'mvideo.ru': 'mvideo',
    'eldorado.ru': 'eldorado',
    'dns-shop.ru': 'dns',
    'citilink.ru': 'citilink',
    'lamoda.ru': 'lamoda',
    'perekrestok.ru': 'perekrestok',
    'magnit.ru': 'magnit',
    'detmir.ru': 'detmir',
    // Операторы связи
    'mts.ru': 'mts',
    'beeline.ru': 'beeline',
    'megafon.ru': 'megafon',
    'tele2.ru': 'tele2',
    'rostelecom.ru': 'rostelecom',
    // Сервисы и развлечения
    'hh.ru': 'hh', 'headhunter.ru': 'hh',
    'superjob.ru': 'superjob',
    '2gis.ru': '2gis',
    'kinopoisk.ru': 'kinopoisk',
    'ivi.ru': 'ivi',
    'netflix.com': 'netflix',
    'spotify.com': 'spotify',
    'apple.com': 'apple', 'icloud.com': 'apple',
    'microsoft.com': 'microsoft',
    'github.com': 'github',
    'paypal.com': 'paypal',
    'booking.com': 'booking',
    'uber.com': 'uber',
    'cdek.ru': 'cdek',
    'pochta.ru': 'pochta', 'russianpost.ru': 'pochta',
    'delivery-club.ru': 'deliveryclub',
    // Прочие сервисы
    'x5paket.ru': 'x5', 'x5.ru': 'x5',
    'ingos.ru': 'ingos',
    '1-ofd.ru': '1ofd',
    'otello.ru': 'otello',
    'vamprivet.ru': 'vamprivet',
};

function getSenderDomain(addr) {
    const m = addr.match(/<([^>]+)>/) || addr.match(/\S+@(\S+)/);
    const email = m ? m[1] || m[0] : addr;
    const at = email.lastIndexOf('@');
    return at >= 0 ? email.slice(at + 1).toLowerCase().trim() : '';
}

function getSenderIconUrl(addr) {
    const domain = getSenderDomain(addr);
    if (!domain) return null;
    // Try full domain first, then walk up subdomains (notify.vk.com → vk.com)
    const parts = domain.split('.');
    for (let i = 0; i < parts.length - 1; i++) {
        const candidate = parts.slice(i).join('.');
        const file = KNOWN_SERVICE_DOMAINS[candidate];
        if (file) return `img/si/${file}.png`;
    }
    return null;
}

// ── Аватар отправителя ──────────────────────────────────────────────────────
const AVATAR_COLORS = [
    '#e53935','#d81b60','#8e24aa','#5e35b1','#3949ab',
    '#1e88e5','#039be5','#00897b','#43a047','#7cb342',
    '#fb8c00','#f4511e','#6d4c41','#546e7a','#00acc1',
];

function senderColor(addr) {
    const raw = (addr.match(/<(.+?)>/) || [])[1] || addr;
    let h = 0;
    for (let i = 0; i < raw.length; i++) h = (Math.imul(h, 31) + raw.charCodeAt(i)) | 0;
    return AVATAR_COLORS[Math.abs(h) % AVATAR_COLORS.length];
}

function senderInitials(addr) {
    if (!addr) return '?';
    const nameRaw = addr.replace(/<.+>/, '').trim().replace(/^"|"$/g, '').trim();
    if (nameRaw) {
        const parts = nameRaw.split(/\s+/);
        return parts.length >= 2
            ? (parts[0][0] + parts[parts.length - 1][0]).toUpperCase()
            : parts[0][0].toUpperCase();
    }
    const email = (addr.match(/<(.+?)>/) || [])[1] || addr;
    return (email[0] || '?').toUpperCase();
}

const _MONTHS_SHORT = ['янв','фев','мар','апр','май','июн','июл','авг','сен','окт','ноя','дек'];
const _dateCache = new Map();
let _todayStr = new Date().toDateString();

function formatDate(dateStr) {
    if (!dateStr) return '';
    const cached = _dateCache.get(dateStr);
    if (cached !== undefined) return cached;
    try {
        const d = new Date(dateStr);
        // Сбрасываем кеш если наступил новый день
        const nowStr = new Date().toDateString();
        if (nowStr !== _todayStr) { _dateCache.clear(); _todayStr = nowStr; }
        let result;
        if (d.toDateString() === _todayStr) {
            // Сегодня — только время, без Intl (быстро)
            result = String(d.getHours()).padStart(2,'0') + ':' + String(d.getMinutes()).padStart(2,'0');
        } else {
            // Другой день — ручной формат без Intl
            result = d.getDate() + ' ' + _MONTHS_SHORT[d.getMonth()];
        }
        _dateCache.set(dateStr, result);
        return result;
    } catch { return dateStr; }
}

// ── Вкладки (Почта / Контакты) ─────────────────────────────────────────────
let currentView = 'mail';
let contacts = [];
let currentContactId = null;
let contactsLoaded = false;
let groups = [];
let currentGroupId = null; // null = все контакты

function switchView(view) {
    currentView = view;
    document.getElementById('mailView').style.display = view === 'mail' ? 'flex' : 'none';
    document.getElementById('contactsView').style.display = view === 'contacts' ? 'flex' : 'none';
    document.getElementById('statusbar').style.display = view === 'mail' ? 'flex' : 'none';
    document.getElementById('tabMail').classList.toggle('active', view === 'mail');
    document.getElementById('tabContacts').classList.toggle('active', view === 'contacts');
    if (view === 'contacts') {
        if (contactsLoaded) {
            renderGroupChips();
            renderContactList(contacts, document.getElementById('contactSearch').value);
        } else {
            loadContacts();
        }
    }
}

// ── Контакты ───────────────────────────────────────────────────────────────
async function loadContacts() {
    try {
        [contacts, groups] = await Promise.all([
            invoke('get_contacts'),
            invoke('get_groups'),
        ]);
        contactsLoaded = true;
        currentGroupId = null;
        renderGroupChips();
        renderContactList(contacts, '');
    } catch (e) {
        document.getElementById('contactsList').innerHTML =
            `<div class="empty-state"><i class="fas fa-exclamation-circle"></i><p>${e}</p></div>`;
    }
}

// ── Группы ─────────────────────────────────────────────────────────────────
function renderGroupChips() {
    let bar = document.getElementById('groupChipsBar');
    if (!bar) return;
    bar.innerHTML = `
        <div class="group-chip ${currentGroupId === null ? 'active' : ''}" onclick="selectGroup(null)">
            Все <span style="opacity:0.6;font-size:11px">${contacts.length}</span>
        </div>
        ${groups.map(g => `
            <div class="group-chip ${currentGroupId === g.id ? 'active' : ''}" onclick="selectGroup(${g.id})">
                ${escHtml(g.name)}
                <span class="group-chip-del" title="Удалить группу"
                      onclick="event.stopPropagation(); deleteGroupConfirm(${g.id}, '${escHtml(g.name).replace(/'/g,"\\'")}')">✕</span>
            </div>
        `).join('')}
        <button class="group-add-btn" id="groupAddBtn" onclick="showGroupInput()">+ Группа</button>
    `;
}

function showGroupInput() {
    const bar = document.getElementById('groupChipsBar');
    const btn = document.getElementById('groupAddBtn');
    if (!btn) return;

    // Обёртка: инпут + кнопка-галочка
    const wrap = document.createElement('div');
    wrap.style.cssText = 'display:flex;align-items:center;gap:3px;';

    const inp = document.createElement('input');
    inp.className = 'group-add-input';
    inp.placeholder = 'Название...';
    inp.maxLength = 40;

    const confirmBtn = document.createElement('button');
    confirmBtn.className = 'group-add-confirm';
    confirmBtn.title = 'Сохранить';
    confirmBtn.innerHTML = '<i class="fas fa-check"></i>';

    wrap.appendChild(inp);
    wrap.appendChild(confirmBtn);
    bar.replaceChild(wrap, btn);
    inp.focus();

    async function saveGroup() {
        const name = inp.value.trim();
        if (name) {
            try {
                const g = await invoke('save_group', { name });
                if (!groups.find(x => x.id === g.id)) groups.push(g);
            } catch (err) { alert('Ошибка: ' + err); }
        }
        renderGroupChips();
    }

    confirmBtn.addEventListener('mousedown', e => e.preventDefault()); // не уводим фокус с инпута
    confirmBtn.addEventListener('click', saveGroup);

    inp.addEventListener('keydown', e => {
        if (e.key === 'Enter') { e.preventDefault(); saveGroup(); }
        else if (e.key === 'Escape') { renderGroupChips(); }
    });
    inp.addEventListener('blur', e => {
        // Не закрываем если клик был по кнопке-галочке
        if (e.relatedTarget === confirmBtn) return;
        setTimeout(renderGroupChips, 150);
    });
}

async function selectGroup(groupId) {
    currentGroupId = groupId;
    renderGroupChips();
    document.getElementById('contactSearch').value = '';
    if (groupId === null) {
        renderContactList(contacts, '');
    } else {
        try {
            const list = await invoke('get_contacts_by_group', { groupId });
            renderContactList(list, '');
        } catch (e) { alert('Ошибка: ' + e); }
    }
}

async function deleteGroupConfirm(groupId, name) {
    if (!await window.__TAURI__.dialog.ask(`Удалить группу "${name}"?\nКонтакты останутся.`, { title: 'Удалить группу', type: 'warning' })) return;
    try {
        await invoke('delete_group', { groupId });
        groups = groups.filter(g => g.id !== groupId);
        if (currentGroupId === groupId) { currentGroupId = null; renderContactList(contacts, ''); }
        renderGroupChips();
    } catch (e) { alert('Ошибка: ' + e); }
}

function filterContacts(term) {
    const q = term.trim().toLowerCase();
    // Если активна группа — поиск идёт по её контактам, иначе по всем
    const base = currentGroupId !== null
        ? contacts.filter(c => c._groupIds && c._groupIds.includes(currentGroupId))
        : contacts;
    const filtered = q ? base.filter(c =>
        c.name.toLowerCase().includes(q) ||
        c.email.toLowerCase().includes(q) ||
        c.company.toLowerCase().includes(q)
    ) : base;
    renderContactList(filtered, q);
}

let selectedContactIds = new Set();

function renderContactList(list, _filter) {
    const el = document.getElementById('contactsList');
    if (list.length === 0) {
        el.innerHTML = '<div class="empty-state"><i class="fas fa-users"></i><p>Нет контактов</p></div>';
        return;
    }
    // Группируем по первой букве
    const grps = {};
    for (const c of list) {
        const key = (c.name || c.email || '?')[0].toUpperCase();
        const letter = /[А-ЯЁA-Z]/.test(key) ? key : '#';
        if (!grps[letter]) grps[letter] = [];
        grps[letter].push(c);
    }
    const letters = Object.keys(grps).sort((a, b) => {
        if (a === '#') return 1;
        if (b === '#') return -1;
        return a.localeCompare(b, 'ru');
    });
    el.innerHTML = letters.map(letter => `
        <div class="contact-group-letter">${letter}</div>
        ${grps[letter].map(c => `
            <div class="contact-list-item ${c.id === currentContactId ? 'active' : ''} ${selectedContactIds.has(c.id) ? 'cli-selected' : ''}"
                 id="cli-${c.id}" onclick="openContact(${c.id})">
                <div class="cli-avatar-zone">
                    <div class="contact-list-avatar">${contactInitial(c)}</div>
                    <input type="checkbox" class="cli-check" ${selectedContactIds.has(c.id) ? 'checked' : ''}
                           onclick="toggleContactCheck(${c.id}, event)" title="Выбрать">
                </div>
                <div class="contact-list-info">
                    <div class="contact-list-name">${escHtml(c.name || c.email)}</div>
                    <div class="contact-list-email">${escHtml(c.email)}</div>
                </div>
            </div>
        `).join('')}
    `).join('');
}

// ── Массовый выбор контактов ────────────────────────────────────────────────
function toggleContactCheck(id, e) {
    e.stopPropagation();
    const row = document.getElementById('cli-' + id);
    if (selectedContactIds.has(id)) {
        selectedContactIds.delete(id);
        if (row) { row.classList.remove('cli-selected'); row.querySelector('.cli-check').checked = false; }
    } else {
        selectedContactIds.add(id);
        if (row) { row.classList.add('cli-selected'); row.querySelector('.cli-check').checked = true; }
    }
    updateContactsSelectBar();
}

function updateContactsSelectBar() {
    const n = selectedContactIds.size;
    const bar = document.getElementById('contactsSelectBar');
    bar.classList.toggle('visible', n > 0);
    document.getElementById('contactsSelCount').textContent = `${n} выбрано`;
    document.getElementById('groupAssignDropdown').style.display = 'none';
    // Sync select-all checkbox
    const allCb = document.getElementById('selectAllContacts');
    if (allCb) {
        const total = document.querySelectorAll('.cli-check').length;
        if (n === 0) { allCb.checked = false; allCb.indeterminate = false; }
        else if (n === total) { allCb.checked = true; allCb.indeterminate = false; }
        else { allCb.checked = false; allCb.indeterminate = true; }
    }
}

function toggleSelectAllContacts(cb) {
    if (cb.checked) {
        document.querySelectorAll('.cli-check').forEach(chk => {
            const row = chk.closest('.contact-list-item');
            const id = parseInt(row.id.replace('cli-', ''));
            if (!selectedContactIds.has(id)) {
                selectedContactIds.add(id);
                row.classList.add('cli-selected');
                chk.checked = true;
            }
        });
    } else {
        clearContactSelection();
        return; // clearContactSelection calls updateContactsSelectBar
    }
    updateContactsSelectBar();
}

function clearContactSelection() {
    selectedContactIds.forEach(id => {
        const row = document.getElementById('cli-' + id);
        if (row) { row.classList.remove('cli-selected'); row.querySelector('.cli-check').checked = false; }
    });
    selectedContactIds.clear();
    updateContactsSelectBar();
}

function toggleGroupAssignDropdown() {
    const dd = document.getElementById('groupAssignDropdown');
    if (dd.style.display !== 'none') { dd.style.display = 'none'; return; }
    if (groups.length === 0) {
        dd.innerHTML = '<div class="group-assign-item" style="color:var(--text2)"><i class="fas fa-info-circle"></i> Нет групп</div>';
    } else {
        dd.innerHTML = groups.map(g =>
            `<div class="group-assign-item" data-gid="${g.id}" onclick="assignSelectedToGroup(${g.id})">
                <i class="fas fa-users"></i> ${escHtml(g.name)}
            </div>`
        ).join('');
    }
    dd.style.display = 'block';
}

async function assignSelectedToGroup(groupId) {
    document.getElementById('groupAssignDropdown').style.display = 'none';
    const ids = [...selectedContactIds];
    if (!ids.length) return;
    try {
        await invoke('add_contacts_to_group', { contactIds: ids, groupId });
        const g = groups.find(x => x.id === groupId);
        clearContactSelection();
        alert(`${ids.length} контакт(ов) добавлено в группу "${g ? g.name : ''}"`);
    } catch (e) { alert('Ошибка: ' + e); }
}

function contactInitial(c) {
    const s = c.name || c.email || '?';
    return s[0].toUpperCase();
}

function openContact(id) {
    currentContactId = id;
    // Подсветим активный
    document.querySelectorAll('.contact-list-item').forEach(el => el.classList.remove('active'));
    const li = document.getElementById('cli-' + id);
    if (li) li.classList.add('active');
    const c = contacts.find(x => x.id === id);
    if (!c) return;
    renderContactDetail(c);
}

async function renderContactDetail(c) {
    // Загружаем группы контакта
    let contactGroupIds = [];
    try { contactGroupIds = await invoke('get_contact_groups', { contactId: c.id }); } catch {}
    const contactGroupNames = groups.filter(g => contactGroupIds.includes(g.id));

    const panel = document.getElementById('contactsDetailPanel');
    panel.innerHTML = `
        <div class="contact-card">
            <div class="contact-card-header">
                <div class="contact-avatar-lg">${contactInitial(c)}</div>
                <div>
                    <div class="contact-name-lg">${escHtml(c.name || c.email)}</div>
                    ${c.company ? `<div class="contact-company-lg">${escHtml(c.company)}</div>` : ''}
                </div>
            </div>
            ${contactGroupNames.length ? `
            <div class="contact-group-badges">
                ${contactGroupNames.map(g => `<span class="contact-group-badge"><i class="fas fa-users" style="font-size:10px"></i> ${escHtml(g.name)}</span>`).join('')}
            </div>` : ''}
            <div class="contact-card-actions">
                <button class="ev-btn" data-email="${escHtml(c.email)}" onclick="openComposeToContact(this.dataset.email)">
                    <i class="fas fa-paper-plane"></i> Написать
                </button>
                <button class="ev-btn" onclick="startEditContact(${c.id})">
                    <i class="fas fa-edit"></i> Изменить
                </button>
                <button class="ev-btn danger" onclick="deleteContactConfirm(${c.id})">
                    <i class="fas fa-trash"></i>
                </button>
            </div>
            <div class="contact-info-rows">
                <div class="contact-info-row">
                    <i class="fas fa-envelope"></i>
                    <span class="ci-label">Email</span>
                    <span class="ci-value ${c.email ? '' : 'empty'}">${escHtml(c.email) || 'Не указан'}</span>
                </div>
                <div class="contact-info-row">
                    <i class="fas fa-phone"></i>
                    <span class="ci-label">Телефон</span>
                    <span class="ci-value ${c.phone ? '' : 'empty'}">${escHtml(c.phone) || 'Не указан'}</span>
                </div>
                <div class="contact-info-row">
                    <i class="fas fa-building"></i>
                    <span class="ci-label">Компания</span>
                    <span class="ci-value ${c.company ? '' : 'empty'}">${escHtml(c.company) || 'Не указана'}</span>
                </div>
            </div>
            ${c.notes ? `
            <div class="contact-notes-block">
                <p><i class="fas fa-sticky-note"></i> Заметки</p>
                <span>${escHtml(c.notes)}</span>
            </div>` : ''}
        </div>
    `;
}

function openComposeToContact(email) {
    switchView('mail');
    openCompose();
    setTimeout(() => {
        document.getElementById('composeTo').value = email;
    }, 50);
}

function newContact() {
    currentContactId = null;
    document.querySelectorAll('.contact-list-item').forEach(el => el.classList.remove('active'));
    renderContactEditForm({ id: 0, name: '', email: '', phone: '', company: '', notes: '' }, true);
}

function startEditContact(id) {
    const c = contacts.find(x => x.id === id);
    if (c) renderContactEditForm(c, false);
}

function cancelContactEdit(btn) {
    if (btn.dataset.isnew === '1') {
        document.getElementById('contactsDetailPanel').innerHTML =
            '<div class="empty-state large"><i class="fas fa-address-book"></i><p>Выберите контакт</p></div>';
    } else {
        openContact(parseInt(btn.dataset.cid));
    }
}

async function renderContactEditForm(c, isNew) {
    let contactGroupIds = [];
    if (!isNew && c.id) {
        try { contactGroupIds = await invoke('get_contact_groups', { contactId: c.id }); } catch {}
    }
    const groupsHtml = groups.length ? `
        <div class="form-row">
            <label>Группы</label>
            <div class="contact-groups-checklist" id="ceGroups">
                ${groups.map(g => `
                    <label>
                        <input type="checkbox" value="${g.id}" ${contactGroupIds.includes(g.id) ? 'checked' : ''}>
                        <span>${escHtml(g.name)}</span>
                    </label>
                `).join('')}
            </div>
        </div>` : '';

    const panel = document.getElementById('contactsDetailPanel');
    panel.innerHTML = `
        <div class="contact-card">
            <div class="contact-card-header">
                <div class="contact-avatar-lg">${c.name ? c.name[0].toUpperCase() : (c.email ? c.email[0].toUpperCase() : '+')}</div>
                <div class="contact-name-lg">${isNew ? 'Новый контакт' : escHtml(c.name || c.email)}</div>
            </div>
            <div class="contact-edit-form">
                <div class="form-row">
                    <label>Имя</label>
                    <input type="text" id="ceName" value="${escHtml(c.name)}" placeholder="Иван Иванов">
                </div>
                <div class="form-row">
                    <label>Email</label>
                    <input type="email" id="ceEmail" value="${escHtml(c.email)}" placeholder="email@example.com">
                </div>
                <div class="form-row">
                    <label>Телефон</label>
                    <input type="text" id="cePhone" value="${escHtml(c.phone)}" placeholder="+7 ...">
                </div>
                <div class="form-row">
                    <label>Компания</label>
                    <input type="text" id="ceCompany" value="${escHtml(c.company)}" placeholder="ООО Ромашка">
                </div>
                ${groupsHtml}
                <div class="form-row">
                    <label>Заметки</label>
                    <textarea id="ceNotes" class="contact-notes-area" placeholder="Заметки...">${escHtml(c.notes)}</textarea>
                </div>
                <div id="ceError" class="setup-error" style="display:none;"></div>
                <div class="contact-edit-actions">
                    <button class="btn-primary" style="width:auto;flex:1;" onclick="saveContactForm(${c.id})">
                        <i class="fas fa-save"></i> Сохранить
                    </button>
                    <button class="ev-btn" data-cid="${c.id}" data-isnew="${isNew ? '1' : '0'}" onclick="cancelContactEdit(this)">
                        Отмена
                    </button>
                </div>
            </div>
        </div>
    `;
}

async function saveContactForm(existingId) {
    const name    = document.getElementById('ceName').value.trim();
    const email   = document.getElementById('ceEmail').value.trim();
    const phone   = document.getElementById('cePhone').value.trim();
    const company = document.getElementById('ceCompany').value.trim();
    const notes   = document.getElementById('ceNotes').value.trim();

    if (!email) {
        const err = document.getElementById('ceError');
        err.textContent = 'Email обязателен'; err.style.display = 'block'; return;
    }

    try {
        const saved = await invoke('save_contact', { contact: { id: existingId, name, email, phone, company, notes } });
        // Сохраняем группы
        const groupChecks = document.querySelectorAll('#ceGroups input[type=checkbox]');
        if (groupChecks.length) {
            const groupIds = [...groupChecks].filter(cb => cb.checked).map(cb => parseInt(cb.value));
            await invoke('set_contact_groups', { contactId: saved.id, groupIds });
        }
        contacts = await invoke('get_contacts');
        renderContactList(contacts, document.getElementById('contactSearch').value);
        currentContactId = saved.id;
        // Подсветим активный после перерисовки
        setTimeout(() => {
            document.querySelectorAll('.contact-list-item').forEach(el => el.classList.remove('active'));
            const li = document.getElementById('cli-' + saved.id);
            if (li) li.classList.add('active');
        }, 0);
        renderContactDetail(saved);
    } catch (e) {
        const err = document.getElementById('ceError');
        if (err) { err.textContent = 'Ошибка: ' + e; err.style.display = 'block'; }
    }
}

async function deleteContactConfirm(id) {
    const c = contacts.find(x => x.id === id);
    if (!c) return;
    if (!await window.__TAURI__.dialog.ask(
        `Удалить контакт "${c.name || c.email}"?`,
        { title: 'Удалить контакт', type: 'warning' }
    )) return;
    try {
        await invoke('delete_contact', { contactId: id });
        contacts = contacts.filter(x => x.id !== id);
        currentContactId = null;
        renderContactList(contacts, document.getElementById('contactSearch').value);
        document.getElementById('contactsDetailPanel').innerHTML =
            '<div class="empty-state large"><i class="fas fa-address-book"></i><p>Выберите контакт</p></div>';
    } catch (e) { alert('Ошибка: ' + e); }
}

function toggleImportDropdown() {
    const dd = document.getElementById('importDropdown');
    const open = dd.style.display !== 'none';
    dd.style.display = open ? 'none' : 'block';
    if (!open) {
        // Закрываем при клике вне
        setTimeout(() => document.addEventListener('click', closeImportDropdownOutside, { once: true }), 0);
    }
}
function closeImportDropdown() {
    document.getElementById('importDropdown').style.display = 'none';
}
function closeImportDropdownOutside(e) {
    if (!e.target.closest('.import-btn-wrap')) closeImportDropdown();
}

async function importContactsVcf(input) {
    const file = input.files[0];
    if (!file) return;
    const text = await file.text();
    input.value = '';
    try {
        const count = await invoke('import_contacts_vcf', { content: text });
        contacts = await invoke('get_contacts');
        contactsLoaded = true;
        renderContactList(contacts, '');
        alert(`Импортировано контактов: ${count}`);
    } catch (e) { alert('Ошибка импорта: ' + e); }
}

async function importContactsCsv(input) {
    const file = input.files[0];
    if (!file) return;
    const text = await file.text();
    input.value = '';
    try {
        const count = await invoke('import_contacts_csv', { content: text });
        contacts = await invoke('get_contacts');
        contactsLoaded = true;
        renderContactList(contacts, '');
        alert(`Импортировано контактов: ${count}`);
    } catch (e) { alert('Ошибка импорта: ' + e); }
}

async function addContactFromEmail(emailId) {
    try {
        const email = await invoke('get_email_body', { emailId });
        const fromAddr = email.from_addr || '';
        // Парсим "Имя <email>" или просто "email@..."
        const m = fromAddr.match(/^"?([^"<]+?)"?\s*<([^>]+)>\s*$/)
               || fromAddr.match(/^([^<>\s]+@[^<>\s]+)$/);
        let name = '', addr = fromAddr.trim();
        if (m && m[2]) { name = m[1].trim(); addr = m[2].trim(); }
        else if (m && m[1]) { addr = m[1].trim(); }
        openAddContactModal(name, addr);
    } catch (e) { alert('Ошибка: ' + e); }
}

function openAddContactModal(name, email) {
    document.getElementById('acName').value = name;
    document.getElementById('acEmail').value = email;
    document.getElementById('acPhone').value = '';
    document.getElementById('acCompany').value = '';
    document.getElementById('acError').style.display = 'none';

    // Группы
    const groupsEl = document.getElementById('acGroups');
    const groupsRow = document.getElementById('acGroupsRow');
    if (groups.length) {
        groupsEl.innerHTML = groups.map(g => `
            <label>
                <input type="checkbox" value="${g.id}"
                    ${g.name === 'Мои контакты' ? 'checked' : ''}>
                <span>${escHtml(g.name)}</span>
            </label>`).join('');
        groupsRow.style.display = 'flex';
    } else {
        groupsRow.style.display = 'none';
    }

    document.getElementById('addContactModal').style.display = 'flex';
    setTimeout(() => document.getElementById('acName').focus(), 50);
}

function closeAddContactModal() {
    document.getElementById('addContactModal').style.display = 'none';
}

function closeAddContactModalOverlay(e) {
    if (e.target === document.getElementById('addContactModal')) closeAddContactModal();
}

async function saveAddContactModal() {
    const name    = document.getElementById('acName').value.trim();
    const email   = document.getElementById('acEmail').value.trim();
    const phone   = document.getElementById('acPhone').value.trim();
    const company = document.getElementById('acCompany').value.trim();

    const errEl = document.getElementById('acError');
    if (!email) { errEl.textContent = 'Email обязателен'; errEl.style.display = 'block'; return; }
    errEl.style.display = 'none';

    const btn = document.getElementById('acSaveBtn');
    btn.disabled = true;
    btn.innerHTML = '<i class="fas fa-spinner fa-spin"></i> Сохранение...';
    try {
        const saved = await invoke('save_contact',
            { contact: { id: 0, name, email, phone, company, notes: '' } });

        const groupChecks = document.querySelectorAll('#acGroups input[type=checkbox]');
        if (groupChecks.length) {
            const groupIds = [...groupChecks].filter(cb => cb.checked).map(cb => parseInt(cb.value));
            await invoke('set_contact_groups', { contactId: saved.id, groupIds });
        }

        const idx = contacts.findIndex(x => x.id === saved.id);
        if (idx >= 0) contacts[idx] = saved; else contacts.push(saved);
        contactsLoaded = true;

        closeAddContactModal();
    } catch (e) {
        errEl.textContent = 'Ошибка: ' + e; errEl.style.display = 'block';
    } finally {
        btn.disabled = false;
        btn.innerHTML = '<i class="fas fa-save"></i> Сохранить';
    }
}

// Санитизация HTML писем через DOMPurify
function sanitizeHtml(html) {
    return DOMPurify.sanitize(html, {
        // Разрешённые теги — всё типичное для email-писем
        ALLOWED_TAGS: [
            'a','abbr','b','blockquote','br','caption','cite','code','col','colgroup',
            'dd','del','details','dfn','div','dl','dt','em','figcaption','figure',
            'footer','h1','h2','h3','h4','h5','h6','head','header','hr','html',
            'i','img','ins','kbd','li','mark','meta','ol','p','pre','q','s',
            'section','small','span','strong','style','sub','summary','sup',
            'table','tbody','td','tfoot','th','thead','time','title','tr','u','ul',
        ],
        ALLOWED_ATTR: [
            'align','alt','bgcolor','border','cellpadding','cellspacing','class',
            'color','colspan','dir','height','href','id','lang','rowspan','scope',
            'src','style','summary','target','title','valign','width',
        ],
        // Разрешаем только http/https/mailto/cid в href и src
        ALLOWED_URI_REGEXP: /^(?:https?|mailto|cid):/i,
        ADD_ATTR: ['target'],
        FORCE_BODY: true,
        WHOLE_DOCUMENT: false,
    });
}

// Хук: принудительно добавляем noopener к внешним ссылкам
DOMPurify.addHook('afterSanitizeAttributes', (node) => {
    if (node.tagName === 'A') {
        node.setAttribute('target', '_blank');
        node.setAttribute('rel', 'noopener noreferrer');
    }
});

// ── Статусная строка ───────────────────────────────────────────────────────
function sbStatus(type, text, newCount) {
    const dot  = document.getElementById('sbDot');
    const msg  = document.getElementById('sbText');
    const last = document.getElementById('sbLastSync');
    const lastT= document.getElementById('sbLastSyncTime');
    const newEl= document.getElementById('sbNewCount');
    const newN = document.getElementById('sbNewNum');
    const sep2 = document.getElementById('sbSep2');
    if (!dot) return;

    dot.className = 'fas fa-circle sb-dot ' + type;
    msg.textContent = text;

    if (type === 'ok') {
        const now = new Date();
        lastT.textContent = now.toLocaleTimeString('ru-RU', { hour: '2-digit', minute: '2-digit' });
        last.style.display = 'flex';
        if (newCount > 0) {
            newN.textContent = newCount;
            newEl.style.display = 'flex';
            sep2.style.display = 'block';
        } else {
            newEl.style.display = 'none';
            sep2.style.display = 'none';
        }
    }
}

// ── Лог-панель ────────────────────────────────────────────────────────────
async function openLogPanel() {
    document.getElementById('logPanel').style.display = 'flex';
    await loadLogs();
}

function closeLogPanel() {
    document.getElementById('logPanel').style.display = 'none';
}

function closeLogPanelOverlay(e) {
    if (e.target === document.getElementById('logPanel')) closeLogPanel();
}

async function loadLogs() {
    const el = document.getElementById('logContent');
    try {
        const text = await invoke('get_log_tail', { lines: 200 });
        el.textContent = text || '(лог пуст)';
        el.scrollTop = el.scrollHeight;
    } catch (e) {
        el.textContent = 'Ошибка: ' + e;
    }
}

async function clearLogs() {
    try {
        await invoke('clear_log');
        document.getElementById('logContent').textContent = '(лог очищен)';
    } catch (e) { alert('Ошибка: ' + e); }
}

// ── Пользовательские папки ───────────────────────────────────────────────────

function renderCustomFolders(folders) {
    const container = document.getElementById('customFoldersList');
    if (!container) return;
    if (folders.length === 0) {
        container.innerHTML = '';
        return;
    }
    container.innerHTML = folders.map(name => `
        <div class="folder-item custom-folder-item ${currentFolder === name ? 'active' : ''}"
             data-folder="${escHtml(name)}" onclick="selectFolder('${escHtml(name)}')">
            <i class="fas fa-folder"></i>
            <span class="custom-folder-name">${escHtml(name)}</span>
            <span class="custom-folder-actions">
                <button class="icon-btn folder-action-btn" title="Переименовать"
                        onclick="openRenameFolder('${escHtml(name)}',event)">
                    <i class="fas fa-pen"></i>
                </button>
                <button class="icon-btn folder-action-btn" title="Удалить"
                        onclick="deleteFolderConfirm('${escHtml(name)}',event)">
                    <i class="fas fa-trash"></i>
                </button>
            </span>
        </div>`).join('');
}

// Загрузить папки из локального кеша (мгновенно)
async function loadCustomFolders() {
    if (!currentAccountId) return;
    try {
        const folders = await invoke('get_cached_folders', { accountId: currentAccountId });
        renderCustomFolders(folders);
    } catch {}
}

// Обновить папки с IMAP-сервера (в фоне, только один раз за сессию на аккаунт)
async function refreshCustomFolders(force = false) {
    if (!currentAccountId) return;
    if (!force && _foldersRefreshed.has(currentAccountId)) return;
    try {
        const folders = await invoke('list_imap_folders', { accountId: currentAccountId });
        renderCustomFolders(folders);
        _foldersRefreshed.add(currentAccountId);
    } catch {}
}

// ── Создание папки ───────────────────────────────────────────────────────────

function promptCreateFolder() {
    const wrap = document.getElementById('newFolderInputWrap');
    wrap.style.display = 'flex';
    const input = document.getElementById('newFolderInput');
    input.value = '';
    setTimeout(() => input.focus(), 50);
}

function cancelCreateFolder() {
    document.getElementById('newFolderInputWrap').style.display = 'none';
    document.getElementById('newFolderInput').value = '';
}

function onNewFolderKey(e) {
    if (e.key === 'Enter') confirmCreateFolder();
    if (e.key === 'Escape') cancelCreateFolder();
}

async function confirmCreateFolder() {
    const name = document.getElementById('newFolderInput').value.trim();
    if (!name) return;
    const btn = document.querySelector('#newFolderInputWrap .fa-check').parentElement;
    btn.disabled = true;
    try {
        await invoke('create_imap_folder', { accountId: currentAccountId, folderName: name });
        cancelCreateFolder();
        await loadCustomFolders();
    } catch (e) {
        alert('Не удалось создать папку: ' + e);
    } finally {
        btn.disabled = false;
    }
}

// ── Удаление папки ───────────────────────────────────────────────────────────

async function deleteFolderConfirm(name, e) {
    e.stopPropagation();
    const ok = await window.__TAURI__.dialog.ask(
        `Удалить папку «${name}»?\nВсе письма в ней будут удалены.`,
        { title: 'Подтверждение', type: 'warning' }
    );
    if (!ok) return;
    try {
        await invoke('delete_imap_folder', { accountId: currentAccountId, folderName: name });
        // Если удаляем текущую — уходим в Входящие
        if (currentFolder === name) selectFolder('INBOX');
        await loadCustomFolders();
    } catch (e) {
        alert('Не удалось удалить папку: ' + e);
    }
}

// ── Переименование папки ─────────────────────────────────────────────────────

function openRenameFolder(name, e) {
    e.stopPropagation();
    renameFolderOldName = name;
    const input = document.getElementById('renameFolderInput');
    input.value = name;
    document.getElementById('renameFolderError').style.display = 'none';
    document.getElementById('renameFolderModal').style.display = 'flex';
    setTimeout(() => { input.focus(); input.select(); }, 50);
}

function closeRenameFolder() {
    document.getElementById('renameFolderModal').style.display = 'none';
    renameFolderOldName = null;
}

function closeRenameFolderOverlay(e) {
    if (e.target === document.getElementById('renameFolderModal')) closeRenameFolder();
}

async function confirmRenameFolder() {
    const newName = document.getElementById('renameFolderInput').value.trim();
    const errEl = document.getElementById('renameFolderError');
    errEl.style.display = 'none';
    if (!newName) { errEl.textContent = 'Введите название'; errEl.style.display = 'block'; return; }
    if (newName === renameFolderOldName) { closeRenameFolder(); return; }
    try {
        await invoke('rename_imap_folder', {
            accountId: currentAccountId,
            oldName: renameFolderOldName,
            newName,
        });
        // Если переименовываем текущую открытую папку — переключаемся на новое имя
        if (currentFolder === renameFolderOldName) {
            currentFolder = newName;
            document.getElementById('folderTitle').textContent = newName;
        }
        closeRenameFolder();
        await loadCustomFolders();
    } catch (e) {
        errEl.textContent = 'Ошибка: ' + e;
        errEl.style.display = 'block';
    }
}
