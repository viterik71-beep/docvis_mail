#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use lettre::{
    message::{header::ContentType, Attachment, MultiPart, SinglePart},
    transport::smtp::authentication::Credentials,
    Message, SmtpTransport, Transport,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as B64};
use mailparse::{parse_mail, MailHeaderMap};
use native_tls::TlsConnector;
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicBool, Ordering};
use tauri::{State, Manager, SystemTray, SystemTrayMenu, SystemTrayEvent, CustomMenuItem};
use chrono::{Datelike, TimeZone};
#[cfg(target_os = "windows")]
use std::os::windows::process::CommandExt;

const CREATE_NO_WINDOW: u32 = 0x08000000;
static ACTIVE_NOTIFS: AtomicU32 = AtomicU32::new(0);
static FULL_SYNC_CANCEL: AtomicBool = AtomicBool::new(false);

// ─── Типы ────────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Account {
    id: i64,
    email: String,
    name: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    password: String,
}

/// Версия Account без пароля — безопасно отдаётся во фронтенд через Tauri bridge
#[derive(Debug, Serialize, Clone)]
struct AccountPublic {
    id: i64,
    email: String,
    name: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    delete_after_days: Option<i64>,
    delete_permanent: bool,
    signature: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct EmailItem {
    id: i64,
    uid: u32,
    folder: String,
    from_addr: String,
    to_addr: String,
    subject: String,
    date: String,
    date_ts: i64,
    is_read: bool,
    is_starred: bool,
    has_attachment: bool,
    snippet: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EmailBody {
    id: i64,
    from_addr: String,
    to_addr: String,
    cc_addr: String,
    subject: String,
    date: String,
    body_html: String,
    body_text: String,
    is_read: bool,
    is_starred: bool,
    has_attachment: bool,
    read_receipt_to: Option<String>,
    read_receipt_sent: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct FileAttachment {
    filename: String,
    data_b64: String,
    mime_type: String,
}

/// Кастомный заголовок lettre для MDN-запроса (RFC 3798)
#[derive(Clone, Debug)]
struct DispositionNotificationTo(String);
impl lettre::message::header::Header for DispositionNotificationTo {
    fn name() -> lettre::message::header::HeaderName {
        lettre::message::header::HeaderName::new_from_ascii_str("Disposition-Notification-To")
    }
    fn parse(s: &str) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Self(s.to_string()))
    }
    fn display(&self) -> lettre::message::header::HeaderValue {
        lettre::message::header::HeaderValue::new(Self::name(), self.0.clone())
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SendMailRequest {
    account_id: i64,
    to: String,
    cc: String,
    subject: String,
    body: String,
    #[serde(default)]
    body_html: Option<String>,
    #[serde(default)]
    attachments: Vec<FileAttachment>,
    #[serde(default)]
    read_receipt: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct AttachmentItem {
    id: i64,
    email_id: i64,
    filename: String,
    mime_type: String,
    file_path: String,
    file_size: i64,
}

struct AppState {
    db: Mutex<Connection>,
}

// ─── Инициализация БД ────────────────────────────────────────────────────────

fn init_db(conn: &Connection) {
    conn.execute_batch("
        CREATE TABLE IF NOT EXISTS accounts (
            id               INTEGER PRIMARY KEY AUTOINCREMENT,
            email            TEXT NOT NULL UNIQUE,
            name             TEXT NOT NULL DEFAULT '',
            imap_host        TEXT NOT NULL,
            imap_port        INTEGER NOT NULL DEFAULT 993,
            smtp_host        TEXT NOT NULL,
            smtp_port        INTEGER NOT NULL DEFAULT 465,
            delete_after_days INTEGER,
            delete_permanent  INTEGER NOT NULL DEFAULT 0,
            signature         TEXT NOT NULL DEFAULT ''
        );

        CREATE TABLE IF NOT EXISTS emails (
            id                INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id        INTEGER NOT NULL,
            uid               INTEGER NOT NULL,
            folder            TEXT    NOT NULL DEFAULT 'INBOX',
            from_addr         TEXT    NOT NULL DEFAULT '',
            to_addr           TEXT    NOT NULL DEFAULT '',
            cc_addr           TEXT    NOT NULL DEFAULT '',
            subject           TEXT    NOT NULL DEFAULT '',
            date              TEXT    NOT NULL DEFAULT '',
            body_text         TEXT    NOT NULL DEFAULT '',
            body_html         TEXT    NOT NULL DEFAULT '',
            is_read           INTEGER NOT NULL DEFAULT 0,
            is_starred        INTEGER NOT NULL DEFAULT 0,
            has_attachment    INTEGER NOT NULL DEFAULT 0,
            message_id        TEXT    NOT NULL DEFAULT '',
            snippet           TEXT    NOT NULL DEFAULT '',
            date_ts           INTEGER NOT NULL DEFAULT 0,
            read_receipt_to   TEXT,
            read_receipt_sent INTEGER NOT NULL DEFAULT 0,
            UNIQUE(account_id, uid, folder)
        );

        CREATE TABLE IF NOT EXISTS attachments (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            email_id   INTEGER NOT NULL,
            filename   TEXT    NOT NULL DEFAULT '',
            mime_type  TEXT    NOT NULL DEFAULT '',
            file_path  TEXT    NOT NULL DEFAULT '',
            file_size  INTEGER NOT NULL DEFAULT 0,
            saved_path TEXT
        );

        CREATE TABLE IF NOT EXISTS contacts (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            name           TEXT    NOT NULL DEFAULT '',
            email          TEXT    NOT NULL DEFAULT '',
            phone          TEXT    NOT NULL DEFAULT '',
            company        TEXT    NOT NULL DEFAULT '',
            notes          TEXT    NOT NULL DEFAULT '',
            is_blacklisted INTEGER NOT NULL DEFAULT 0,
            UNIQUE(email)
        );

        CREATE TABLE IF NOT EXISTS contact_groups (
            id   INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE
        );

        CREATE TABLE IF NOT EXISTS contact_group_members (
            contact_id INTEGER NOT NULL,
            group_id   INTEGER NOT NULL,
            PRIMARY KEY (contact_id, group_id)
        );

        CREATE TABLE IF NOT EXISTS blocked_senders (
            id    INTEGER PRIMARY KEY AUTOINCREMENT,
            email TEXT NOT NULL UNIQUE COLLATE NOCASE
        );

        CREATE TABLE IF NOT EXISTS folder_state (
            account_id INTEGER NOT NULL,
            folder     TEXT    NOT NULL,
            last_uid   INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (account_id, folder)
        );

        CREATE TABLE IF NOT EXISTS user_folders (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            account_id INTEGER NOT NULL,
            name       TEXT    NOT NULL,
            UNIQUE(account_id, name)
        );

        CREATE INDEX IF NOT EXISTS idx_emails_list
        ON emails(account_id, folder, date_ts DESC);
    ").expect("DB init failed");

    // Группа по умолчанию
    conn.execute(
        "INSERT OR IGNORE INTO contact_groups (name) VALUES ('Мои контакты')",
        [],
    ).ok();
}

fn default_group_id(conn: &Connection) -> Option<i64> {
    conn.query_row(
        "SELECT id FROM contact_groups WHERE name='Мои контакты'",
        [], |r| r.get(0),
    ).ok()
}

fn add_to_default_group(conn: &Connection, contact_id: i64) {
    if let Some(gid) = default_group_id(conn) {
        conn.execute(
            "INSERT OR IGNORE INTO contact_group_members (contact_id, group_id) VALUES (?1, ?2)",
            params![contact_id, gid],
        ).ok();
    }
}

fn get_data_dir() -> std::path::PathBuf {
    let base = dirs_next::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("docvis_mail");
    std::fs::create_dir_all(&base).ok();
    base
}

// ─── Windows Credential Manager ──────────────────────────────────────────────

const KEYRING_SERVICE: &str = "docvis_mail";

fn cred_set(email: &str, password: &str) -> Result<(), String> {
    keyring::Entry::new(KEYRING_SERVICE, email)
        .map_err(|e| e.to_string())?
        .set_password(password)
        .map_err(|e| e.to_string())
}

fn cred_get(email: &str) -> Option<String> {
    keyring::Entry::new(KEYRING_SERVICE, email).ok()?.get_password().ok()
}

fn cred_delete(email: &str) {
    if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, email) {
        let _ = entry.delete_password();
    }
}

// ─── Резервное копирование ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct BackupSettings {
    enabled: bool,
    interval_days: u32,        // 1=ежедневно, 7=еженедельно, 30=ежемесячно
    last_backup: Option<String>, // RFC3339 timestamp
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self { enabled: false, interval_days: 7, last_backup: None }
    }
}

fn backup_settings_path() -> std::path::PathBuf {
    get_data_dir().join("backup_settings.json")
}

fn read_backup_settings() -> BackupSettings {
    std::fs::read_to_string(backup_settings_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn write_backup_settings(s: &BackupSettings) -> Result<(), String> {
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(backup_settings_path(), json).map_err(|e| e.to_string())
}

fn backups_dir() -> std::path::PathBuf {
    let dir = get_data_dir().join("backups");
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn cleanup_old_backups(keep: usize) {
    let dir = backups_dir();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x == "db").unwrap_or(false))
            .collect();
        files.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());
        if files.len() > keep {
            for f in &files[..files.len() - keep] {
                let _ = std::fs::remove_file(f.path());
            }
        }
    }
}

fn do_backup_internal() -> Result<String, String> {
    let db_path = get_db_path();
    let dir = backups_dir();
    let ts = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let backup_path = dir.join(format!("mail_{}.db", ts));
    std::fs::copy(&db_path, &backup_path).map_err(|e| e.to_string())?;
    cleanup_old_backups(10);
    let mut settings = read_backup_settings();
    settings.last_backup = Some(chrono::Local::now().to_rfc3339());
    let _ = write_backup_settings(&settings);
    log_to_file(&format!("Backup created: {}", backup_path.display()));
    Ok(backup_path.to_string_lossy().to_string())
}

#[tauri::command]
fn backup_database(state: State<'_, AppState>) -> Result<String, String> {
    // WAL checkpoint: фиксируем все данные в основном файле перед копированием
    {
        let conn = state.db.lock().map_err(|e| e.to_string())?;
        conn.execute_batch("PRAGMA wal_checkpoint(FULL)").map_err(|e| e.to_string())?;
    }
    do_backup_internal()
}

#[tauri::command]
fn restore_database(source_path: String) -> Result<(), String> {
    let src = std::path::PathBuf::from(&source_path);
    if !src.exists() {
        return Err("Файл не найден".to_string());
    }
    // Проверяем сигнатуру SQLite
    let header = std::fs::read(&src).map_err(|e| e.to_string())?;
    if header.len() < 16 || &header[..16] != b"SQLite format 3\0" {
        return Err("Файл не является базой данных SQLite".to_string());
    }
    // Сохраняем путь к файлу — восстановление произойдёт при следующем запуске
    let pending = get_data_dir().join("pending_restore.db");
    std::fs::copy(&src, &pending).map_err(|e| e.to_string())?;
    log_to_file("Restore pending. App will apply on next start.");
    Ok(())
}

#[tauri::command]
fn get_backup_settings() -> BackupSettings {
    read_backup_settings()
}

#[tauri::command]
fn set_backup_settings(enabled: bool, interval_days: u32) -> Result<(), String> {
    // Читаем текущие настройки чтобы сохранить last_backup
    let mut settings = read_backup_settings();
    settings.enabled = enabled;
    settings.interval_days = interval_days;
    write_backup_settings(&settings)
}

#[tauri::command]
fn get_backups_dir_path() -> String {
    backups_dir().to_string_lossy().to_string()
}

#[tauri::command]
fn open_backups_folder() -> Result<(), String> {
    let dir = backups_dir();
    std::process::Command::new("explorer")
        .arg(dir.to_string_lossy().to_string())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn restart_app(app_handle: tauri::AppHandle) {
    tauri::api::process::restart(&app_handle.env());
}

fn log_to_file(msg: &str) {
    use std::io::Write;
    let path = get_data_dir().join("mail.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

/// Ротация лога: если mail.log > 5 МБ — переименовываем в mail.log.old
fn rotate_log_if_needed() {
    let path = get_data_dir().join("mail.log");
    const MAX_SIZE: u64 = 5 * 1024 * 1024; // 5 МБ
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.len() > MAX_SIZE {
            let old = get_data_dir().join("mail.log.old");
            let _ = std::fs::rename(&path, &old);
        }
    }
}

fn get_db_path() -> std::path::PathBuf {
    get_data_dir().join("mail.db")
}

fn attachments_dir(email_id: i64) -> std::path::PathBuf {
    let dir = get_data_dir().join("attachments").join(email_id.to_string());
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn extract_filename(cd: &str, ct_header: &str) -> String {
    // filename= из Content-Disposition
    for part in cd.split(';') {
        let part = part.trim();
        let low = part.to_lowercase();
        if low.starts_with("filename=") {
            let name = part[9..].trim().trim_matches('"').trim();
            if !name.is_empty() { return sanitize_filename(name); }
        }
    }
    // name= из Content-Type
    for part in ct_header.split(';') {
        let part = part.trim();
        if part.to_lowercase().starts_with("name=") {
            let name = part[5..].trim().trim_matches('"').trim();
            if !name.is_empty() { return sanitize_filename(name); }
        }
    }
    "attachment".to_string()
}

fn sanitize_filename(name: &str) -> String {
    let s: String = name.chars()
        .filter(|c| !matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    let s = s.trim().to_string();
    // Предотвращаем path traversal: ".." или "." как имя файла
    if s == ".." || s == "." || s.is_empty() {
        return "attachment".to_string();
    }
    s
}

fn collect_attachments(
    part: &mailparse::ParsedMail,
    result: &mut Vec<(String, String, Vec<u8>)>,
) {
    let cd = part.headers.get_first_value("Content-Disposition").unwrap_or_default();
    let ct_header = part.headers.get_first_value("Content-Type").unwrap_or_default();
    let ct = part.ctype.mimetype.to_lowercase();
    let cd_lower = cd.to_lowercase();

    let has_filename = cd_lower.contains("filename") || ct_header.to_lowercase().contains("name=");
    let is_attach = cd_lower.contains("attachment") || (has_filename && !ct.starts_with("multipart/"));

    if is_attach && part.subparts.is_empty() {
        if let Ok(raw) = part.get_body_raw() {
            if !raw.is_empty() {
                let filename = extract_filename(&cd, &ct_header);
                result.push((filename, ct, raw));
            }
        }
        return;
    }

    for sub in &part.subparts {
        collect_attachments(sub, result);
    }
}

// ─── Вспомогательные функции ─────────────────────────────────────────────────

fn snippet_from_text(text: &str, max: usize) -> String {
    let trimmed: String = text.chars()
        .filter(|c| *c != '\r' && *c != '\n')
        .collect();
    let trimmed = trimmed.trim().to_string();
    // chars().take() — безопасно для кириллицы и любого Unicode
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        trimmed
    } else {
        format!("{}…", chars[..max].iter().collect::<String>())
    }
}

// ─── Очистка старых писем с сервера ──────────────────────────────────────────

fn purge_old_emails_imap(account: &Account, uids: &[u32], permanent: bool) -> Result<(), String> {
    use std::net::TcpStream;
    if uids.is_empty() { return Ok(()); }

    let timeout = std::time::Duration::from_secs(30);
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port))
        .map_err(|e| e.to_string())?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    let tls = native_tls::TlsConnector::builder().build().map_err(|e| e.to_string())?;
    let tls_stream = tls.connect(&account.imap_host, tcp).map_err(|e| e.to_string())?;
    let client = imap::Client::new(tls_stream);
    let mut session = client.login(&account.email, &account.password)
        .map_err(|(e, _)| e.to_string())?;

    session.select("INBOX").map_err(|e| e.to_string())?;

    let uid_set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");

    if !permanent {
        // Ищем папку Trash на сервере
        let trash = ["Trash", "INBOX.Trash", "[Gmail]/Корзина", "[Gmail]/Trash",
                     "Удалённые", "Deleted Messages", "Deleted Items"]
            .iter()
            .find(|&&name| session.list(None, Some(name)).map(|l| !l.is_empty()).unwrap_or(false))
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Trash".to_string());

        // Копируем в корзину
        session.uid_mv(&uid_set, &trash).ok();
    }

    // Помечаем \Deleted и удаляем
    session.uid_store(&uid_set, "+FLAGS (\\Deleted)").map_err(|e| e.to_string())?;
    session.expunge().map_err(|e| e.to_string())?;
    let _ = session.logout();
    Ok(())
}

fn purge_old_emails_for_account(conn: &Connection, account: &Account,
                                 days: i64, permanent: bool) -> usize {
    let cutoff = chrono::Utc::now().timestamp() - days * 86400;

    // Получаем UID писем из INBOX старше N дней
    let mut stmt = match conn.prepare(
        "SELECT id, uid FROM emails WHERE account_id=?1 AND folder='INBOX' AND date_ts > 0 AND date_ts < ?2"
    ) {
        Ok(s) => s,
        Err(_) => return 0,
    };

    let rows: Vec<(i64, u32)> = stmt.query_map(params![account.id, cutoff], |r| {
        Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?))
    }).ok()
    .map(|iter| iter.filter_map(|r| r.ok()).collect())
    .unwrap_or_default();

    if rows.is_empty() { return 0; }

    let uids: Vec<u32> = rows.iter().map(|(_, uid)| *uid).collect();

    // Удаляем с сервера через IMAP
    if let Err(e) = purge_old_emails_imap(account, &uids, permanent) {
        log_to_file(&format!("purge_old_emails: IMAP error for {}: {}", account.email, e));
        return 0;
    }

    log_to_file(&format!("purge_old_emails: removed {} emails from server for {}", uids.len(), account.email));
    uids.len()
}

// ─── IMAP: получение писем ───────────────────────────────────────────────────

/// Возвращает (письма, uid_next) — uid_next нужен для корректной инициализации last_uid
fn fetch_from_imap(account: &Account, folder: &str, limit: u32, offset: u32, leave_on_server: bool) -> Result<(Vec<(u32, Vec<u8>)>, u32), String> {
    use std::net::TcpStream;

    log_to_file(&format!("fetch_from_imap: folder={} offset={}", folder, offset));

    let timeout = std::time::Duration::from_secs(30);
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port))
        .map_err(|e| { let s = format!("TCP connect error: {}", e); log_to_file(&s); s })?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();

    let tls = TlsConnector::builder().build()
        .map_err(|e| { let s = format!("TLS build error: {}", e); log_to_file(&s); s })?;
    let tls_stream = tls.connect(&account.imap_host, tcp)
        .map_err(|e| { let s = format!("TLS connect error: {}", e); log_to_file(&s); s })?;

    let client = imap::Client::new(tls_stream);
    let mut session = client
        .login(&account.email, &account.password)
        .map_err(|(e, _)| { let s = format!("IMAP login error: {}", e); log_to_file(&s); s })?;

    let mailbox = session.select(folder)
        .map_err(|e| { let s = format!("IMAP select error: {}", e); log_to_file(&s); s })?;
    let count = mailbox.exists;
    // uid_next — следующий UID, который сервер выдаст. Текущий max UID = uid_next - 1
    let uid_next = mailbox.uid_next.unwrap_or(0);
    log_to_file(&format!("fetch_from_imap: exists={} uid_next={}", count, uid_next));

    if count == 0 {
        let _ = session.logout();
        return Ok((vec![], uid_next));
    }

    // offset > 0 означает "загрузить ещё" (пагинация по seq, старые письма)
    // "UID RFC822" — просим сервер вернуть реальный UID вместе с телом письма,
    // иначе msg.uid = None и мы используем seq-номер как UID, что ломает
    // инкрементальный sync на серверах где UID >> seq (mail.ru).
    let messages = if offset > 0 {
        if count <= offset { let _ = session.logout(); return Ok((vec![], uid_next)); }
        let end = count - offset;
        let start = if end > limit { end - limit + 1 } else { 1 };
        log_to_file(&format!("fetch_from_imap: load-more seq {}:{}", start, end));
        session.fetch(format!("{}:{}", start, end), "(UID RFC822)")
            .map_err(|e| { let s = format!("IMAP fetch error: {}", e); log_to_file(&s); s })?
    } else {
        let end = count;
        let start = if end > limit { end - limit + 1 } else { 1 };
        log_to_file(&format!("fetch_from_imap: initial seq {}:{}", start, end));
        session.fetch(format!("{}:{}", start, end), "(UID RFC822)")
            .map_err(|e| { let s = format!("IMAP fetch error: {}", e); log_to_file(&s); s })?
    };

    let mut result: Vec<(u32, Vec<u8>)> = Vec::new();
    for msg in messages.iter() {
        if let Some(body) = msg.body() {
            result.push((msg.uid.unwrap_or(msg.message), body.to_vec()));
        }
    }

    log_to_file(&format!("fetch_from_imap: fetched {} messages", result.len()));
    if !leave_on_server && !result.is_empty() {
        let uid_set: String = result.iter().map(|(u, _)| u.to_string()).collect::<Vec<_>>().join(",");
        if session.uid_store(&uid_set, "+FLAGS (\\Deleted)").is_ok() {
            session.expunge().ok();
            log_to_file(&format!("fetch_from_imap: deleted {} UIDs from server", result.len()));
        }
    }
    let _ = session.logout();
    Ok((result, uid_next))
}

/// Инкрементальный IMAP-fetch: только письма с UID > last_uid
fn fetch_new_from_imap(account: &Account, folder: &str, last_uid: u32, leave_on_server: bool) -> Result<Vec<(u32, Vec<u8>)>, String> {
    use std::net::TcpStream;

    log_to_file(&format!("fetch_new_from_imap: folder={} last_uid={}", folder, last_uid));

    let timeout = std::time::Duration::from_secs(30);
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port))
        .map_err(|e| { let s = format!("TCP connect error: {}", e); log_to_file(&s); s })?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();

    let tls = TlsConnector::builder().build()
        .map_err(|e| { let s = format!("TLS build error: {}", e); log_to_file(&s); s })?;
    let tls_stream = tls.connect(&account.imap_host, tcp)
        .map_err(|e| { let s = format!("TLS connect error: {}", e); log_to_file(&s); s })?;

    let client = imap::Client::new(tls_stream);
    let mut session = client
        .login(&account.email, &account.password)
        .map_err(|(e, _)| { let s = format!("IMAP login error: {}", e); log_to_file(&s); s })?;

    let mailbox = session.select(folder)
        .map_err(|e| { let s = format!("IMAP select error: {}", e); log_to_file(&s); s })?;

    if mailbox.exists == 0 {
        let _ = session.logout();
        return Ok(vec![]);
    }

    // Сначала UID SEARCH — быстрый запрос, сервер возвращает только список UID
    // Намного быстрее чем UID FETCH X:* когда новых писем нет
    let search_criteria = format!("UID {}:*", last_uid + 1);
    let found_uids = session.uid_search(&search_criteria)
        .map_err(|e| { let s = format!("IMAP uid_search error: {}", e); log_to_file(&s); s })?;

    // Фильтруем: убираем last_uid (сервер может вернуть его при запросе last_uid+1:*)
    let mut new_uids: Vec<u32> = found_uids.into_iter().filter(|&u| u > last_uid).collect();
    new_uids.sort_unstable(); // от старых к новым

    let total_new = new_uids.len();
    log_to_file(&format!("fetch_new_from_imap: found {} new UIDs", total_new));

    if new_uids.is_empty() {
        let _ = session.logout();
        return Ok(vec![]);
    }

    // Ограничиваем батч: берём первые (самые старые) MAX_BATCH писем.
    // При большом кол-ве новых (напр. mail.ru после долгого перерыва) это
    // не позволяет отправить один огромный uid_fetch и получить os error 10054.
    // Следующий sync подберёт следующую порцию и т.д.
    const MAX_BATCH: usize = 100;
    new_uids.truncate(MAX_BATCH);
    if total_new > MAX_BATCH {
        log_to_file(&format!(
            "fetch_new_from_imap: capped to {} (total new: {}), next sync will fetch more",
            MAX_BATCH, total_new
        ));
    }

    // uid_fetch чанками по 50 — mail.ru обрывает соединение на больших запросах
    const FETCH_CHUNK: usize = 50;
    let mut result: Vec<(u32, Vec<u8>)> = Vec::new();
    for chunk in new_uids.chunks(FETCH_CHUNK) {
        let uid_list = chunk.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        let messages = session.uid_fetch(&uid_list, "RFC822")
            .map_err(|e| { let s = format!("IMAP uid_fetch error: {}", e); log_to_file(&s); s })?;
        for msg in messages.iter() {
            if let Some(uid) = msg.uid {
                if let Some(body) = msg.body() {
                    result.push((uid, body.to_vec()));
                }
            }
        }
    }

    log_to_file(&format!("fetch_new_from_imap: fetched {} messages", result.len()));
    if !leave_on_server && !result.is_empty() {
        let uid_set: String = result.iter().map(|(u, _)| u.to_string()).collect::<Vec<_>>().join(",");
        if session.uid_store(&uid_set, "+FLAGS (\\Deleted)").is_ok() {
            session.expunge().ok();
            log_to_file(&format!("fetch_new_from_imap: deleted {} UIDs from server", result.len()));
        }
    }
    let _ = session.logout();
    Ok(result)
}

// ─── IMAP: управление папками ────────────────────────────────────────────────

/// Декодирует Modified UTF-7 (RFC 3501) → UTF-8.
/// Пример: "&BB4EQgQ,BEAEMAQyB..." → "Отправленные"
fn decode_imap_utf7(input: &str) -> String {
    let mut result = String::new();
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'&' {
            result.push(bytes[i] as char);
            i += 1;
            continue;
        }
        i += 1; // skip '&'
        if i < bytes.len() && bytes[i] == b'-' {
            result.push('&'); // "&-" → literal '&'
            i += 1;
            continue;
        }
        // Collect base64 chars until '-'
        let start = i;
        while i < bytes.len() && bytes[i] != b'-' { i += 1; }
        let b64_slice = &input[start..i];
        if i < bytes.len() { i += 1; } // skip '-'

        // Modified UTF-7 uses ',' instead of '/' in base64 alphabet
        let b64_std: String = b64_slice.chars().map(|c| if c == ',' { '/' } else { c }).collect();
        // base64 decode (no padding in modified UTF-7)
        let decoded = {
            let mut table = [0xFFu8; 128];
            for (idx, &c) in b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/".iter().enumerate() {
                table[c as usize] = idx as u8;
            }
            let vals: Vec<u8> = b64_std.bytes()
                .filter(|&b| (b as usize) < 128 && table[b as usize] != 0xFF)
                .map(|b| table[b as usize])
                .collect();
            let mut out = Vec::new();
            for chunk in vals.chunks(4) {
                match chunk.len() {
                    4 => { out.push((chunk[0]<<2)|(chunk[1]>>4)); out.push(((chunk[1]&0xF)<<4)|(chunk[2]>>2)); out.push(((chunk[2]&3)<<6)|chunk[3]); }
                    3 => { out.push((chunk[0]<<2)|(chunk[1]>>4)); out.push(((chunk[1]&0xF)<<4)|(chunk[2]>>2)); }
                    2 => { out.push((chunk[0]<<2)|(chunk[1]>>4)); }
                    _ => {}
                }
            }
            out
        };
        // Interpret bytes as UTF-16BE → push decoded chars
        let utf16: Vec<u16> = decoded.chunks(2)
            .filter(|c| c.len() == 2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        if let Ok(s) = String::from_utf16(&utf16) {
            result.push_str(&s);
        } else {
            // fallback: keep original
            result.push('&'); result.push_str(b64_slice); result.push('-');
        }
    }
    result
}

/// Системные папки (английские и русские названия) — скрываем из списка пользователя
const SYSTEM_FOLDERS: &[&str] = &[
    "INBOX", "Sent", "Drafts", "Spam", "Trash", "Draft", "Junk", "Deleted",
    // Типичные русские названия системных папок mail.ru после декодирования
    "Отправленные", "Черновики", "Удалённые", "Удаленные", "Спам",
    "Корзина", "Входящие", "Исходящие",
];

fn is_system_folder(name: &str) -> bool {
    SYSTEM_FOLDERS.iter().any(|s| s.eq_ignore_ascii_case(name))
}

/// Открывает TLS IMAP-сессию без SELECT папки (для управляющих команд)
fn imap_session(account: &Account) -> Result<imap::Session<native_tls::TlsStream<std::net::TcpStream>>, String> {
    use std::net::TcpStream;
    let timeout = std::time::Duration::from_secs(30);
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port))
        .map_err(|e| format!("TCP connect: {}", e))?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    let tls = native_tls::TlsConnector::builder().build()
        .map_err(|e| format!("TLS build: {}", e))?;
    let tls_stream = tls.connect(&account.imap_host, tcp)
        .map_err(|e| format!("TLS connect: {}", e))?;
    let client = imap::Client::new(tls_stream);
    client.login(&account.email, &account.password)
        .map_err(|(e, _)| format!("IMAP login: {}", e))
}

/// Возвращает список пользовательских папок из кеша (быстро, без IMAP)
#[tauri::command]
fn get_cached_folders(state: State<AppState>, account_id: i64) -> Result<Vec<String>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT name FROM user_folders WHERE account_id=?1 ORDER BY name"
    ).map_err(|e| e.to_string())?;
    let names: Vec<String> = stmt.query_map(params![account_id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();
    Ok(names)
}

/// Запрашивает папки с IMAP-сервера и обновляет кеш. Возвращает полный список.
#[tauri::command]
fn list_imap_folders(state: State<AppState>, account_id: i64) -> Result<Vec<String>, String> {
    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let mut session = imap_session(&account)?;
    let mailboxes = session.list(Some(""), Some("*"))
        .map_err(|e| format!("LIST error: {}", e))?;

    let mut folders: Vec<String> = Vec::new();
    for m in mailboxes.iter() {
        // Пропускаем папки с атрибутом \NoSelect (нельзя выбрать, только контейнер)
        if m.attributes().iter().any(|a| matches!(a, imap::types::NameAttribute::NoSelect)) {
            continue;
        }
        // Декодируем имя из Modified UTF-7
        let decoded = decode_imap_utf7(m.name());
        // Пропускаем системные папки
        if is_system_folder(&decoded) { continue; }
        // Пропускаем делегированные ящики (содержат '@') — это чужие ящики mail.ru
        if decoded.contains('@') { continue; }
        folders.push(decoded);
    }
    folders.sort();

    let _ = session.logout();

    // Обновляем кеш
    let conn = state.db.lock().unwrap();
    conn.execute("DELETE FROM user_folders WHERE account_id=?1", params![account_id]).ok();
    for name in &folders {
        conn.execute(
            "INSERT OR IGNORE INTO user_folders (account_id, name) VALUES (?1,?2)",
            params![account_id, name],
        ).ok();
    }

    Ok(folders)
}

/// Создаёт папку на IMAP-сервере и сохраняет в кеш
#[tauri::command]
fn create_imap_folder(state: State<AppState>, account_id: i64, folder_name: String) -> Result<(), String> {
    if folder_name.trim().is_empty() {
        return Err("Имя папки не может быть пустым".to_string());
    }
    if is_system_folder(&folder_name) {
        return Err("Нельзя создать папку с зарезервированным именем".to_string());
    }

    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let mut session = imap_session(&account)?;
    session.create(&folder_name)
        .map_err(|e| format!("Ошибка создания папки: {}", e))?;
    let _ = session.logout();

    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO user_folders (account_id, name) VALUES (?1,?2)",
        params![account_id, folder_name],
    ).map_err(|e| e.to_string())?;

    Ok(())
}

/// Удаляет папку на IMAP-сервере и из кеша
#[tauri::command]
fn delete_imap_folder(state: State<AppState>, account_id: i64, folder_name: String) -> Result<(), String> {
    if is_system_folder(&folder_name) {
        return Err("Системные папки нельзя удалять".to_string());
    }

    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let mut session = imap_session(&account)?;
    session.delete(&folder_name)
        .map_err(|e| format!("Ошибка удаления папки: {}", e))?;
    let _ = session.logout();

    let conn = state.db.lock().unwrap();
    conn.execute(
        "DELETE FROM user_folders WHERE account_id=?1 AND name=?2",
        params![account_id, folder_name],
    ).ok();
    // Удаляем локальные письма из этой папки
    conn.execute(
        "DELETE FROM emails WHERE account_id=?1 AND folder=?2",
        params![account_id, folder_name],
    ).ok();

    Ok(())
}

/// Переименовывает папку на IMAP-сервере и в кеше
#[tauri::command]
fn rename_imap_folder(state: State<AppState>, account_id: i64, old_name: String, new_name: String) -> Result<(), String> {
    if is_system_folder(&old_name) {
        return Err("Системные папки нельзя переименовывать".to_string());
    }
    if new_name.trim().is_empty() {
        return Err("Имя папки не может быть пустым".to_string());
    }

    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let mut session = imap_session(&account)?;
    session.rename(&old_name, &new_name)
        .map_err(|e| format!("Ошибка переименования: {}", e))?;
    let _ = session.logout();

    let conn = state.db.lock().unwrap();
    conn.execute(
        "UPDATE user_folders SET name=?1 WHERE account_id=?2 AND name=?3",
        params![new_name, account_id, old_name],
    ).ok();
    conn.execute(
        "UPDATE emails SET folder=?1 WHERE account_id=?2 AND folder=?3",
        params![new_name, account_id, old_name],
    ).ok();

    Ok(())
}

/// Перемещает выбранные письма в указанную папку (IMAP UID MOVE + DB)
#[tauri::command]
fn move_emails_to_folder(
    state: State<AppState>,
    account_id: i64,
    email_ids: Vec<i64>,
    target_folder: String,
) -> Result<(), String> {
    if email_ids.is_empty() {
        return Ok(());
    }

    // Получаем uid и исходную папку для каждого письма (пропускаем уже в target)
    let rows: Vec<(i64, u32, String)> = {
        let conn = state.db.lock().unwrap();
        let mut result = Vec::new();
        for id in &email_ids {
            if let Ok(row) = conn.query_row(
                "SELECT id, uid, folder FROM emails WHERE id=?1 AND account_id=?2",
                params![id, account_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, u32>(1)?, r.get::<_, String>(2)?)),
            ) {
                if row.2 != target_folder {
                    result.push(row);
                }
            }
        }
        result
    };

    if rows.is_empty() {
        return Ok(());
    }

    // Группируем uid по исходной папке
    let mut by_folder: std::collections::HashMap<String, Vec<u32>> = std::collections::HashMap::new();
    for (_, uid, folder) in &rows {
        by_folder.entry(folder.clone()).or_default().push(*uid);
    }

    // Получаем аккаунт
    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let mut session = imap_session(&account)?;
    for (src_folder, uids) in &by_folder {
        session.select(src_folder)
            .map_err(|e| format!("SELECT {}: {}", src_folder, e))?;
        let uid_set = uids.iter().map(|u| u.to_string()).collect::<Vec<_>>().join(",");
        // Пробуем IMAP MOVE (RFC 6851), при ошибке — COPY + \Deleted + EXPUNGE
        if session.uid_mv(&uid_set, &target_folder).is_err() {
            session.uid_copy(&uid_set, &target_folder)
                .map_err(|e| format!("UID COPY: {}", e))?;
            session.uid_store(&uid_set, "+FLAGS.SILENT (\\Deleted)")
                .map_err(|e| format!("UID STORE: {}", e))?;
            session.expunge()
                .map_err(|e| format!("EXPUNGE: {}", e))?;
        }
    }
    let _ = session.logout();

    // Обновляем DB
    {
        let conn = state.db.lock().unwrap();
        for id in &email_ids {
            conn.execute(
                "UPDATE emails SET folder=?1 WHERE id=?2 AND account_id=?3",
                params![target_folder, id, account_id],
            ).ok();
        }
    }
    log_to_file(&format!(
        "move_emails_to_folder: moved {} emails to '{}'",
        email_ids.len(), target_folder
    ));
    Ok(())
}

/// Прогрев PowerShell/WPF при запуске — чтобы первое уведомление появлялось быстро
#[tauri::command]
fn prewarm_powershell() {
    std::thread::spawn(|| {
        let _ = std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden",
                   "-Command", "Add-Type -AssemblyName PresentationFramework"])
            .creation_flags(CREATE_NO_WINDOW)
            .output();
    });
}

/// WPF-уведомление о новом письме (PowerShell, borderless окно, стекинг снизу-справа)
#[tauri::command]
fn show_mail_notification(
    app_handle: tauri::AppHandle,
    from_name: String,
    subject: String,
    email_id: i64,
    duration_secs: Option<u32>,
) -> Result<(), String> {
    let stack_index = ACTIVE_NOTIFS.fetch_add(1, Ordering::SeqCst);
    let duration = duration_secs.unwrap_or(5); // 0 = не скрывать автоматически

    let temp_path = std::env::temp_dir()
        .join(format!("docvis_mail_notif_{}.ps1", stack_index));

    // Данные передаём через переменные окружения — никакой интерполяции в код скрипта.
    // Это полностью исключает инъекцию через имя отправителя или тему письма.
    let mut script = String::new();
    script.push_str("Add-Type -AssemblyName PresentationFramework\n");
    script.push_str("Add-Type -AssemblyName PresentationCore\n");
    script.push_str("$fromName = $env:NOTIF_FROM\n");
    script.push_str("$subject  = $env:NOTIF_SUBJECT\n");
    script.push_str(&format!("$stackIndex = {}\n", stack_index));
    script.push_str(&format!("$notifDuration = {}\n", duration));
    script.push_str(MAIL_NOTIF_BODY);

    let mut content = vec![0xEF, 0xBB, 0xBF]; // UTF-8 BOM (для кириллицы)
    content.extend_from_slice(script.as_bytes());
    std::fs::write(&temp_path, content).map_err(|e| e.to_string())?;

    let ps_path = temp_path.to_string_lossy().to_string();
    let app = app_handle.clone();

    std::thread::spawn(move || {
        let result = std::process::Command::new("powershell")
            .args(["-ExecutionPolicy", "Bypass", "-WindowStyle", "Hidden",
                   "-File", &ps_path])
            .env("NOTIF_FROM",    &from_name)
            .env("NOTIF_SUBJECT", &subject)
            .creation_flags(CREATE_NO_WINDOW)
            .output();

        ACTIVE_NOTIFS.fetch_sub(1, Ordering::SeqCst);
        let _ = std::fs::remove_file(&ps_path);

        if let Ok(output) = result {
            let code = output.status.code();
            if let Some(window) = app.get_window("main") {
                if code == Some(1) {
                    // Кликнули по телу — открываем письмо
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                    let _ = window.emit("open-email-from-notification", email_id);
                } else if code == Some(2) {
                    // Кликнули на шестерёнку — открываем настройки уведомлений
                    let _ = window.show();
                    let _ = window.unminimize();
                    let _ = window.set_focus();
                    let _ = window.emit("open-notification-settings", ());
                }
            }
        }
    });

    Ok(())
}

const MAIL_NOTIF_BODY: &str = r##"
[xml]$xaml = '<Window xmlns="http://schemas.microsoft.com/winfx/2006/xaml/presentation" WindowStyle="None" AllowsTransparency="True" Background="Transparent" Topmost="True" ShowInTaskbar="False" Width="340" Height="90" ResizeMode="NoResize"><Border Background="#FF1A1B2E" BorderBrush="#FF6366F1" BorderThickness="1" CornerRadius="8" Name="MainBorder"><Grid Margin="10,8"><Grid.ColumnDefinitions><ColumnDefinition Width="40"/><ColumnDefinition Width="*"/><ColumnDefinition Width="44"/></Grid.ColumnDefinitions><Border Grid.Column="0" Width="32" Height="32" CornerRadius="16" Background="#FF6366F1" VerticalAlignment="Center"><TextBlock Foreground="White" FontSize="16" FontWeight="Bold" HorizontalAlignment="Center" VerticalAlignment="Center" Name="AvatarText"/></Border><StackPanel Grid.Column="1" Margin="8,0,0,0" VerticalAlignment="Center"><TextBlock Foreground="#FF9999BB" FontSize="11" Name="TitleBlock"/><TextBlock Foreground="White" FontWeight="SemiBold" FontSize="13" TextTrimming="CharacterEllipsis" Name="SenderBlock"/><TextBlock Foreground="#FFCCCCCC" FontSize="12" TextTrimming="CharacterEllipsis" Name="MsgBlock"/></StackPanel><StackPanel Grid.Column="2" VerticalAlignment="Top" HorizontalAlignment="Center" Margin="0,2,0,0"><TextBlock Text="&#215;" Foreground="#FF666666" FontSize="16" HorizontalAlignment="Center" Cursor="Hand" Name="CloseBtn"/><TextBlock Text="&#9881;" Foreground="#FF666666" FontSize="13" HorizontalAlignment="Center" Cursor="Hand" Margin="0,4,0,0" Name="SettingsBtn"/></StackPanel></Grid></Border></Window>'
$reader = New-Object System.Xml.XmlNodeReader $xaml
$window = [Windows.Markup.XamlReader]::Load($reader)
$window.FindName('TitleBlock').Text = 'ДокВис Почта · ' + (Get-Date).ToString('HH:mm')
$window.FindName('SenderBlock').Text = $fromName
$window.FindName('MsgBlock').Text = $subject
if ($fromName.Length -gt 0) {
    $window.FindName('AvatarText').Text = $fromName.Substring(0, 1).ToUpper()
}

# Позиция: правый нижний угол, стекинг вверх
$screen = [System.Windows.SystemParameters]::WorkArea
$window.Left = $screen.Right - 352
$window.Top = $screen.Bottom - 95 - ($stackIndex * 98)

# Клик по телу → открыть письмо
$global:clicked = $false
$global:settingsClicked = $false
$window.FindName('MainBorder').Add_MouseLeftButtonDown({
    if (-not $global:settingsClicked) { $global:clicked = $true; $window.Close() }
})

# Кнопка закрытия ×
$closeBtn = $window.FindName('CloseBtn')
$closeBtn.Add_MouseLeftButtonDown({ param($s,$e) $e.Handled = $true; $window.Close() })
$closeBtn.Add_MouseEnter({ $closeBtn.Foreground = [System.Windows.Media.Brushes]::White })
$defColor = New-Object System.Windows.Media.SolidColorBrush ([System.Windows.Media.Color]::FromRgb(0x66,0x66,0x66))
$closeBtn.Add_MouseLeave({ $closeBtn.Foreground = $defColor })

# Кнопка настроек ⚙
$settingsBtn = $window.FindName('SettingsBtn')
$settingsBtn.Add_MouseLeftButtonDown({ param($s,$e) $e.Handled = $true; $global:settingsClicked = $true; $window.Close() })
$settingsBtn.Add_MouseEnter({ $settingsBtn.Foreground = [System.Windows.Media.Brushes]::White })
$settingsBtn.Add_MouseLeave({ $settingsBtn.Foreground = $defColor })

# Автозакрытие (если $notifDuration = 0 — не скрывать автоматически)
if ($notifDuration -gt 0) {
    $timer = New-Object System.Windows.Threading.DispatcherTimer
    $timer.Interval = [TimeSpan]::FromSeconds($notifDuration)
    $timer.Add_Tick({ $timer.Stop(); $window.Close() })
    $timer.Start()
}

$window.ShowDialog() | Out-Null
if ($global:settingsClicked) { exit 2 } elseif ($global:clicked) { exit 1 } else { exit 0 }
"##;

/// Извлекает email-адрес из строки вида "Имя <email>" или просто "email"
fn extract_email_addr(from: &str) -> String {
    if let (Some(lt), Some(gt)) = (from.find('<'), from.find('>')) {
        from[lt + 1..gt].trim().to_lowercase()
    } else {
        from.trim().to_lowercase()
    }
}

/// Проверяет, заблокирован ли отправитель
fn is_sender_blocked(conn: &Connection, from: &str) -> bool {
    let addr = extract_email_addr(from);
    if addr.is_empty() { return false; }
    conn.query_row(
        "SELECT 1 FROM blocked_senders WHERE email = ?1 COLLATE NOCASE",
        params![addr],
        |_| Ok(()),
    ).is_ok()
}

/// Возвращает Some(email_id) если письмо новое, None если уже было
fn parse_and_store(
    conn: &Connection,
    account_id: i64,
    folder: &str,
    uid: u32,
    raw: &[u8],
) -> Result<Option<i64>, String> {
    log_to_file(&format!("parse_and_store: uid={} folder={} raw_len={}", uid, folder, raw.len()));
    let mail = parse_mail(raw).map_err(|e| {
        let s = format!("parse_mail error uid={}: {}", uid, e);
        log_to_file(&s); s
    })?;
    let headers = &mail.headers;

    let from          = headers.get_first_value("From").unwrap_or_default();
    let to            = headers.get_first_value("To").unwrap_or_default();
    let cc            = headers.get_first_value("Cc").unwrap_or_default();
    let subject       = headers.get_first_value("Subject").unwrap_or_default();
    let date          = headers.get_first_value("Date").unwrap_or_default();
    let message_id    = headers.get_first_value("Message-ID").unwrap_or_default();
    // RFC 3798 MDN: отправитель запрашивает уведомление о прочтении
    let read_receipt_to = headers.get_first_value("Disposition-Notification-To")
        .or_else(|| headers.get_first_value("Return-Receipt-To"))
        .filter(|s| !s.is_empty());

    let mut body_text = String::new();
    let mut body_html = String::new();
    let mut has_attachment = false;

    fn collect_parts(
        part: &mailparse::ParsedMail,
        text: &mut String,
        html: &mut String,
        attach: &mut bool,
    ) {
        let ct = part.ctype.mimetype.to_lowercase();
        let cd = part.headers
            .get_first_value("Content-Disposition")
            .unwrap_or_default()
            .to_lowercase();

        if cd.contains("attachment") {
            *attach = true;
            return;
        }

        if ct == "text/plain" && text.is_empty() {
            *text = part.get_body().unwrap_or_default();
        } else if ct == "text/html" && html.is_empty() {
            *html = part.get_body().unwrap_or_default();
        } else {
            for sub in &part.subparts {
                collect_parts(sub, text, html, attach);
            }
        }
    }

    // Собираем вложения
    let mut raw_attachments: Vec<(String, String, Vec<u8>)> = Vec::new();
    collect_attachments(&mail, &mut raw_attachments);
    has_attachment = has_attachment || !raw_attachments.is_empty();

    collect_parts(&mail, &mut body_text, &mut body_html, &mut has_attachment);

    // Если отправитель в чёрном списке — кладём в Спам независимо от исходной папки
    let effective_folder = if folder == "INBOX" && is_sender_blocked(conn, &from) {
        "Spam"
    } else {
        folder
    };

    // Предвычисляем snippet (100 символов из body_text, или из body_html без тегов)
    let snippet_src = if !body_text.is_empty() {
        body_text.as_str()
    } else {
        &body_html
    };
    // Грубо убираем HTML-теги без regex (char-by-char)
    let plain: String = if body_text.is_empty() && !body_html.is_empty() {
        let mut out = String::with_capacity(snippet_src.len());
        let mut in_tag = false;
        for c in snippet_src.chars() {
            match c {
                '<' => in_tag = true,
                '>' => { in_tag = false; out.push(' '); }
                _ if !in_tag => out.push(c),
                _ => {}
            }
        }
        out
    } else {
        body_text.clone()
    };
    let snippet = snippet_from_text(&plain, 100);

    // Парсим дату в Unix timestamp для быстрой сортировки
    let date_ts: i64 = mailparse::dateparse(&date).unwrap_or(0);

    // Если письмо уже существует в любой папке (включая Spam после блокировки) — пропускаем.
    // Это предотвращает бесконечный цикл: письмо заблокировано → переехало в Spam →
    // следующий sync снова вставляет его с folder='INBOX' → UNIQUE(uid,folder) не срабатывает.
    let already_exists: bool = conn.query_row(
        "SELECT 1 FROM emails WHERE account_id=?1 AND uid=?2 LIMIT 1",
        params![account_id, uid as i64],
        |_| Ok(()),
    ).is_ok();
    if already_exists {
        return Ok(None);
    }

    let changed = conn.execute(
        "INSERT OR IGNORE INTO emails
         (account_id, uid, folder, from_addr, to_addr, cc_addr, subject, date,
          body_text, body_html, has_attachment, message_id, snippet, date_ts, read_receipt_to)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15)",
        params![
            account_id, uid as i64, effective_folder,
            from, to, cc, subject, date,
            body_text, body_html, has_attachment as i32, message_id,
            snippet, date_ts, read_receipt_to
        ],
    ).map_err(|e| e.to_string())?;

    log_to_file(&format!("parse_and_store: uid={} inserted={} subject={:?}", uid, changed, subject));

    // Если письмо новое — возвращаем его ID и сохраняем вложения
    if changed > 0 {
        let email_id = conn.last_insert_rowid();

        if !raw_attachments.is_empty() {
            let dir = attachments_dir(email_id);

            for (filename, mime_type, data) in &raw_attachments {
                // Уникальное имя файла на случай дублей
                let safe_name = if filename.is_empty() || filename == "attachment" {
                    format!("attachment_{}.bin", chrono::Local::now().timestamp_millis())
                } else {
                    filename.clone()
                };
                let file_path = dir.join(&safe_name);

                if std::fs::write(&file_path, data).is_ok() {
                    let path_str = file_path.to_string_lossy().to_string();
                    conn.execute(
                        "INSERT INTO attachments (email_id, filename, mime_type, file_path, file_size)
                         VALUES (?1,?2,?3,?4,?5)",
                        params![email_id, safe_name, mime_type, path_str, data.len() as i64],
                    ).ok();
                }
            }
        }

        return Ok(Some(email_id));
    }

    Ok(None)
}

// ─── Tauri команды ───────────────────────────────────────────────────────────

/// Читает аккаунт из БД и подставляет пароль из Windows Credential Manager
fn get_account_by_id(conn: &Connection, id: i64) -> Result<Account, String> {
    let mut acc = conn.query_row(
        "SELECT id,email,name,imap_host,imap_port,smtp_host,smtp_port FROM accounts WHERE id=?1",
        params![id],
        |r| Ok(Account {
            id: r.get(0)?, email: r.get(1)?, name: r.get(2)?,
            imap_host: r.get(3)?, imap_port: r.get(4)?,
            smtp_host: r.get(5)?, smtp_port: r.get(6)?,
            password: String::new(),
        }),
    ).map_err(|e| e.to_string())?;
    acc.password = cred_get(&acc.email).unwrap_or_default();
    Ok(acc)
}

#[tauri::command]
fn get_accounts(state: State<AppState>) -> Result<Vec<AccountPublic>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, email, name, imap_host, imap_port, smtp_host, smtp_port, delete_after_days, delete_permanent, signature FROM accounts"
    ).map_err(|e| e.to_string())?;

    let list: Vec<AccountPublic> = stmt.query_map([], |r| Ok(AccountPublic {
        id:                r.get(0)?,
        email:             r.get(1)?,
        name:              r.get(2)?,
        imap_host:         r.get(3)?,
        imap_port:         r.get(4)?,
        smtp_host:         r.get(5)?,
        smtp_port:         r.get(6)?,
        delete_after_days: r.get(7)?,
        delete_permanent:  r.get::<_, i32>(8)? != 0,
        signature:         r.get(9).unwrap_or_default(),
    })).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .collect();

    Ok(list)
}

fn verify_imap(email: &str, password: &str, host: &str, port: u16) -> Result<(), String> {
    use std::net::TcpStream;
    let timeout = std::time::Duration::from_secs(20);
    let tcp = TcpStream::connect((host, port))
        .map_err(|e| format!("Не удалось подключиться к IMAP-серверу {}: {}", host, e))?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    let tls = TlsConnector::builder().build()
        .map_err(|e| format!("TLS ошибка: {}", e))?;
    let tls_stream = tls.connect(host, tcp)
        .map_err(|e| format!("TLS подключение к {}: {}", host, e))?;
    let client = imap::Client::new(tls_stream);
    let mut session = client
        .login(email, password)
        .map_err(|(e, _)| format!("Ошибка авторизации: {}", e))?;
    let _ = session.logout();
    Ok(())
}

#[tauri::command]
fn save_account(
    state: State<AppState>,
    email: String,
    name: String,
    password: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    delete_after_days: Option<i64>,
    delete_permanent: bool,
) -> Result<(), String> {
    verify_imap(&email, &password, &imap_host, imap_port)?;
    cred_set(&email, &password)?;

    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO accounts (email, name, imap_host, imap_port, smtp_host, smtp_port, delete_after_days, delete_permanent)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(email) DO UPDATE SET
           name=excluded.name,
           imap_host=excluded.imap_host, imap_port=excluded.imap_port,
           smtp_host=excluded.smtp_host, smtp_port=excluded.smtp_port,
           delete_after_days=excluded.delete_after_days,
           delete_permanent=excluded.delete_permanent",
        params![email, name, imap_host, imap_port as i64, smtp_host, smtp_port as i64,
                delete_after_days, delete_permanent as i32],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn update_account(
    state: State<AppState>,
    account_id: i64,
    name: String,
    password: String,
    imap_host: String,
    imap_port: u16,
    smtp_host: String,
    smtp_port: u16,
    delete_after_days: Option<i64>,
    delete_permanent: bool,
    signature: String,
) -> Result<(), String> {
    // Получаем email (идентификатор не меняется)
    let email: String = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT email FROM accounts WHERE id=?1",
            params![account_id],
            |r| r.get(0),
        ).map_err(|_| "Аккаунт не найден".to_string())?
    };

    // Если пользователь не менял пароль (поле пустое) — берём из Keyring
    let actual_password = if password.is_empty() {
        cred_get(&email).ok_or_else(|| "Пароль не найден в хранилище".to_string())?
    } else {
        password
    };

    // Проверяем подключение с актуальными данными до сохранения
    verify_imap(&email, &actual_password, &imap_host, imap_port)?;

    // Сохраняем пароль в Windows Credential Manager
    cred_set(&email, &actual_password)?;

    let conn = state.db.lock().unwrap();
    conn.execute(
        "UPDATE accounts SET name=?1, imap_host=?2, imap_port=?3, smtp_host=?4, smtp_port=?5,
         delete_after_days=?6, delete_permanent=?7, signature=?8 WHERE id=?9",
        params![name, imap_host, imap_port as i64, smtp_host, smtp_port as i64,
                delete_after_days, delete_permanent as i32, signature, account_id],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_account(state: State<AppState>, account_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();

    // Получаем email перед удалением, чтобы убрать credential
    let email: Option<String> = conn.query_row(
        "SELECT email FROM accounts WHERE id=?1", params![account_id], |r| r.get(0)
    ).ok();

    conn.execute("DELETE FROM emails WHERE account_id=?1", params![account_id])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM accounts WHERE id=?1", params![account_id])
        .map_err(|e| e.to_string())?;

    // Удаляем пароль из Credential Manager
    if let Some(email) = email {
        cred_delete(&email);
    }

    Ok(())
}

#[derive(serde::Serialize, Clone)]
struct NotifItem {
    id: i64,
    from_addr: String,
    subject: String,
    has_attachment: bool,
}

#[tauri::command]
/// Перемещает в Спам все письма аккаунта, чей from_addr совпадает с заблокированным контактом.
/// Вызывается после сохранения новых писем в sync_folder.
fn filter_blacklisted_to_spam(conn: &Connection, account_id: i64) {
    conn.execute(
        "UPDATE emails SET folder='Spam'
         WHERE account_id=?1 AND folder!='Spam'
           AND EXISTS (
               SELECT 1 FROM contacts
               WHERE is_blacklisted=1
                 AND INSTR(LOWER(emails.from_addr), LOWER(contacts.email)) > 0
                 AND LENGTH(contacts.email) > 0
           )",
        params![account_id],
    ).ok();
}

#[tauri::command]
async fn sync_folder(
    app_handle: tauri::AppHandle,
    state: State<'_, AppState>,
    account_id: i64,
    folder: String,
    offset: Option<u32>,
    leave_on_server: Option<bool>,
) -> Result<Vec<NotifItem>, String> {
    let offset = offset.unwrap_or(0);
    let leave_on_server = leave_on_server.unwrap_or(true);
    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    // Получаем last_uid для инкрементальной синхронизации (только при offset=0)
    let last_uid: u32 = if offset == 0 {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT last_uid FROM folder_state WHERE account_id=?1 AND folder=?2",
            params![account_id, &folder],
            |r| r.get::<_, i64>(0),
        ).unwrap_or(0) as u32
    } else { 0 };

    let folder_clone = folder.clone();
    // (messages, uid_next_hint) — uid_next_hint > 0 только при первичной загрузке
    let fetch_result = tauri::async_runtime::spawn_blocking(move || {
        if offset == 0 && last_uid > 0 {
            // Инкрементальный режим: только новые по UID
            fetch_new_from_imap(&account, &folder_clone, last_uid, leave_on_server)
                .map(|msgs| (msgs, 0u32))
        } else {
            // Первичная загрузка или "загрузить ещё"
            fetch_from_imap(&account, &folder_clone, 100, offset, leave_on_server)
        }
    }).await.map_err(|e| e.to_string())?;

    // Если папка не существует на сервере — тихо возвращаем пустой список
    let (messages, uid_next_hint) = match fetch_result {
        Ok(val) => val,
        Err(ref e) if e.contains("NONEXISTENT") || e.to_lowercase().contains("not exist") => {
            log_to_file(&format!("sync_folder: folder '{}' not found on server, skipping", folder));
            return Ok(vec![]);
        }
        Err(e) => return Err(e),
    };

    log_to_file(&format!("sync_folder: received {} messages uid_next_hint={}", messages.len(), uid_next_hint));

    let mut all_new_items: Vec<NotifItem> = Vec::new();
    let mut max_uid: u32 = last_uid;
    {
        let conn = state.db.lock().unwrap();
        for (uid, raw) in &messages {
            if *uid > max_uid { max_uid = *uid; }
            match parse_and_store(&conn, account_id, &folder, *uid, raw) {
                Ok(Some(eid)) => {
                    // Читаем полный EmailItem — нужен и для события, и для NotifItem
                    if let Ok(item) = conn.query_row(
                        "SELECT id, uid, folder, from_addr, to_addr, subject, date, date_ts,
                                is_read, is_starred, has_attachment, snippet
                         FROM emails WHERE id=?1",
                        params![eid],
                        |r| Ok(EmailItem {
                            id:             r.get(0)?,
                            uid:            r.get::<_, i64>(1)? as u32,
                            folder:         r.get(2)?,
                            from_addr:      r.get(3)?,
                            to_addr:        r.get(4)?,
                            subject:        r.get(5)?,
                            date:           r.get(6)?,
                            date_ts:        r.get(7)?,
                            is_read:        r.get::<_, i32>(8)? != 0,
                            is_starred:     r.get::<_, i32>(9)? != 0,
                            has_attachment: r.get::<_, i32>(10)? != 0,
                            snippet:        r.get(11)?,
                        }),
                    ) {
                        all_new_items.push(NotifItem {
                            id: item.id,
                            from_addr: item.from_addr.clone(),
                            subject: item.subject.clone(),
                            has_attachment: item.has_attachment,
                        });
                        // Реалтайм-событие → JS добавляет письмо в список по одному
                        let _ = app_handle.emit_all("email-received", &item);
                    }
                }
                Ok(None) => {}
                Err(e) => log_to_file(&format!("sync_folder: error uid={}: {}", uid, e)),
            }
        }

        // При первичной загрузке (last_uid==0, offset==0) используем uid_next-1 как last_uid,
        // чтобы при следующей синхронизации не тащить ВСЕ письма с uid > max_uid_из_выборки.
        // mail.ru: sequence order ≠ UID order, поэтому max UID последних 100 писем
        // может быть намного меньше реального максимального UID ящика.
        let effective_last_uid = if last_uid == 0 && offset == 0 && uid_next_hint > 0 {
            let hint = uid_next_hint.saturating_sub(1);
            log_to_file(&format!("sync_folder: initial sync, using uid_next-1={} as last_uid (max_uid from fetch={})", hint, max_uid));
            hint
        } else {
            max_uid
        };

        // Обновляем last_uid для следующей синхронизации
        if effective_last_uid > last_uid {
            conn.execute(
                "INSERT INTO folder_state (account_id, folder, last_uid) VALUES (?1,?2,?3)
                 ON CONFLICT(account_id, folder) DO UPDATE SET last_uid=excluded.last_uid",
                params![account_id, &folder, effective_last_uid as i64],
            ).ok();
            log_to_file(&format!("sync_folder: updated last_uid to {}", effective_last_uid));
        }

        // Письма от заблокированных контактов → Спам
        if !messages.is_empty() {
            filter_blacklisted_to_spam(&conn, account_id);
        }
    }

    // Очистка старых писем (только при инкрементальном синхронизации INBOX, не при первичной загрузке)
    if folder == "INBOX" && last_uid > 0 && offset == 0 {
        let account_clone = {
            let conn = state.db.lock().unwrap();
            get_account_by_id(&conn, account_id).ok()
        };
        if let Some(acc) = account_clone {
            let (days_opt, permanent) = {
                let conn = state.db.lock().unwrap();
                let r: Option<(Option<i64>, bool)> = conn.query_row(
                    "SELECT delete_after_days, delete_permanent FROM accounts WHERE id=?1",
                    params![account_id],
                    |r| Ok((r.get(0)?, r.get::<_, i32>(1)? != 0)),
                ).ok();
                r.unwrap_or((None, false))
            };
            if let Some(days) = days_opt {
                let conn = state.db.lock().unwrap();
                purge_old_emails_for_account(&conn, &acc, days, permanent);
            }
        }
    }

    // При первичной загрузке (last_uid был 0) не возвращаем элементы —
    // это исторические письма, уведомлять о них не нужно.
    // При инкрементальном sync (last_uid > 0) возвращаем все новые элементы.
    if last_uid == 0 && offset == 0 {
        Ok(vec![])
    } else {
        Ok(all_new_items)
    }
}

/// Полная синхронизация: забирает ВСЕ письма с сервера, 100 за раз,
/// пока не дойдёт до самого старого. Эмитит событие full-sync-progress.
#[tauri::command]
async fn full_sync_folder(
    app_handle: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
    account_id: i64,
    folder: String,
    leave_on_server: bool,
) -> Result<u32, String> {
    use std::net::TcpStream;

    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    log_to_file(&format!("full_sync_folder: account={} folder={} leave={}", account.email, folder, leave_on_server));

    // Одно IMAP-соединение на весь процесс
    let timeout = std::time::Duration::from_secs(60);
    let tcp = TcpStream::connect((account.imap_host.as_str(), account.imap_port))
        .map_err(|e| format!("TCP connect: {}", e))?;
    tcp.set_read_timeout(Some(timeout)).ok();
    tcp.set_write_timeout(Some(timeout)).ok();
    let tls = native_tls::TlsConnector::builder().build()
        .map_err(|e| format!("TLS build: {}", e))?;
    let tls_stream = tls.connect(&account.imap_host, tcp)
        .map_err(|e| format!("TLS connect: {}", e))?;
    let client = imap::Client::new(tls_stream);
    let mut session = client.login(&account.email, &account.password)
        .map_err(|(e, _)| format!("IMAP login: {}", e))?;

    let mailbox = session.select(&folder)
        .map_err(|e| format!("IMAP select: {}", e))?;
    let total_msgs = mailbox.exists;

    if total_msgs == 0 {
        let _ = session.logout();
        return Ok(0);
    }

    log_to_file(&format!("full_sync_folder: total={} messages on server", total_msgs));

    FULL_SYNC_CANCEL.store(false, Ordering::Relaxed);
    let batch_size: u32 = 100;
    let mut offset: u32 = 0;
    let mut total_inserted: u32 = 0;

    loop {
        if total_msgs <= offset { break; }
        if FULL_SYNC_CANCEL.load(Ordering::Relaxed) {
            log_to_file("full_sync_folder: cancelled by user");
            break;
        }

        let end = total_msgs - offset;
        let start = if end > batch_size { end - batch_size + 1 } else { 1 };
        log_to_file(&format!("full_sync_folder: seq {}:{} (offset={})", start, end, offset));

        let messages = match session.fetch(format!("{}:{}", start, end), "(UID RFC822)") {
            Ok(m) => m,
            Err(e) => {
                log_to_file(&format!("full_sync_folder: fetch error: {}", e));
                break;
            }
        };

        let mut batch: Vec<(u32, Vec<u8>)> = Vec::new();
        for msg in messages.iter() {
            if let Some(body) = msg.body() {
                batch.push((msg.uid.unwrap_or(msg.message), body.to_vec()));
            }
        }

        if batch.is_empty() { break; }

        let (batch_inserted, new_ids) = {
            let conn = state.db.lock().unwrap();
            let mut count: u32 = 0;
            let mut ids: Vec<i64> = Vec::new();
            for (uid, raw) in &batch {
                match parse_and_store(&conn, account_id, &folder, *uid, raw) {
                    Ok(Some(eid)) => { count += 1; ids.push(eid); }
                    _ => {}
                }
            }
            (count, ids)
        };

        if !leave_on_server {
            let uid_set: String = batch.iter().map(|(u, _)| u.to_string()).collect::<Vec<_>>().join(",");
            if session.uid_store(&uid_set, "+FLAGS (\\Deleted)").is_ok() {
                session.expunge().ok();
            }
        }

        total_inserted += batch_inserted;
        offset += batch_size;

        let fetched_so_far = offset.min(total_msgs);
        let cancelled = FULL_SYNC_CANCEL.load(Ordering::Relaxed);
        let _ = app_handle.emit_all("full-sync-progress", serde_json::json!({
            "fetched":   fetched_so_far,
            "total":     total_msgs,
            "inserted":  total_inserted,
            "new_ids":   new_ids,
            "cancelled": cancelled,
        }));

        if cancelled { break; }
    }

    let _ = session.logout();
    log_to_file(&format!("full_sync_folder: done inserted={}/{}", total_inserted, total_msgs));
    Ok(total_inserted)
}

#[tauri::command]
fn cancel_full_sync() {
    FULL_SYNC_CANCEL.store(true, Ordering::Relaxed);
    log_to_file("cancel_full_sync: requested");
}

#[tauri::command]
fn auto_save_attachments(
    state: State<AppState>,
    email_ids: Vec<i64>,
    save_base: String,
    neutralize: Option<bool>,
) -> Result<(), String> {
    if email_ids.is_empty() { return Ok(()); }

    let do_neutralize = neutralize.unwrap_or(true);

    // Если путь не задан — используем Рабочий стол / Почта
    let base = if save_base.is_empty() {
        dirs_next::desktop_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join("Почта")
    } else {
        std::path::PathBuf::from(&save_base)
    };

    let conn = state.db.lock().unwrap();

    for email_id in &email_ids {
        // Берём дату из письма, а не с компьютера
        let date_ts: i64 = conn.query_row(
            "SELECT date_ts FROM emails WHERE id=?1",
            params![email_id],
            |r| r.get(0),
        ).unwrap_or_else(|_| chrono::Local::now().timestamp());

        let email_dt = chrono::Local.timestamp_opt(date_ts, 0)
            .single()
            .unwrap_or_else(|| chrono::Local::now());

        let month_ru = match email_dt.month() {
            1=>"январь", 2=>"февраль", 3=>"март", 4=>"апрель",
            5=>"май", 6=>"июнь", 7=>"июль", 8=>"август",
            9=>"сентябрь", 10=>"октябрь", 11=>"ноябрь", _=>"декабрь",
        };
        let dest_dir = base
            .join(email_dt.format("%Y").to_string())
            .join(month_ru)
            .join(email_dt.format("%d").to_string())
            .join(email_dt.format("%H-%M").to_string());
        let mut stmt = match conn.prepare(
            "SELECT id, file_path, filename FROM attachments WHERE email_id=?1 AND saved_path IS NULL"
        ) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let rows: Vec<(i64, String, String)> = match stmt.query_map(params![email_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        }) {
            Ok(mapped) => mapped.filter_map(|r| r.ok()).collect(),
            Err(_) => continue,
        };

        if rows.is_empty() { continue; }
        if std::fs::create_dir_all(&dest_dir).is_err() { continue; }

        for (att_id, file_path, filename) in rows {
            let src = std::path::Path::new(&file_path);
            if !src.exists() { continue; }
            let dest_name = if do_neutralize {
                if is_dangerous_attach_ext(&filename) {
                    neutralize_filename(&filename)
                } else {
                    // Для ZIP/7z/RAR — проверяем содержимое на опасные файлы
                    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
                    if matches!(ext.as_str(), "zip" | "7z" | "rar") {
                        let scan = scan_archive_by_ext(&file_path, &ext);
                        if !scan.dangerous.is_empty() || scan.encrypted {
                            neutralize_filename(&filename)
                        } else {
                            filename.clone()
                        }
                    } else {
                        filename.clone()
                    }
                }
            } else {
                filename.clone()
            };
            let dest = dest_dir.join(&dest_name);
            if std::fs::copy(src, &dest).is_ok() {
                let saved = dest.to_string_lossy().to_string();
                // Удаляем внутреннюю копию — она больше не нужна
                std::fs::remove_file(src).ok();
                conn.execute(
                    "UPDATE attachments SET saved_path=?1, file_path=?1 WHERE id=?2",
                    params![saved, att_id],
                ).ok();
            }
        }
    }
    Ok(())
}

#[tauri::command]
fn get_emails(
    state: State<AppState>,
    account_id: i64,
    folder: String,
    limit: Option<i64>,
    offset: Option<i64>,
    filter: Option<String>,
    sort: Option<String>,
) -> Result<Vec<EmailItem>, String> {
    let limit = limit.unwrap_or(50).min(500);
    let offset = offset.unwrap_or(0).max(0);

    let filter_clause = match filter.as_deref().unwrap_or("all") {
        "unread"      => " AND is_read = 0",
        "attachments" => " AND has_attachment = 1",
        _             => "",
    };
    let order_clause = match sort.as_deref().unwrap_or("date_desc") {
        "date_asc"     => "date_ts ASC, id ASC",
        "from_asc"     => "u_lower(from_addr) ASC, date_ts DESC",
        "from_desc"    => "u_lower(from_addr) DESC, date_ts DESC",
        "subject_asc"  => "u_lower(subject) ASC, date_ts DESC",
        "subject_desc" => "u_lower(subject) DESC, date_ts DESC",
        _              => "date_ts DESC, id DESC",
    };
    let sql = format!(
        "SELECT id, uid, folder, from_addr, to_addr, subject, date, date_ts,
                is_read, is_starred, has_attachment, snippet
         FROM emails
         WHERE account_id=?1 AND folder=?2{}
         ORDER BY {}
         LIMIT ?3 OFFSET ?4",
        filter_clause, order_clause
    );

    let conn = state.db.lock().unwrap();
    // snippet уже в БД — не читаем body_text (экономим на чтении больших полей)
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;

    let list: Vec<EmailItem> = stmt.query_map(params![account_id, folder, limit, offset], |r| {
        // Для старых записей без snippet — берём заглушку; snippet_from_text не вызываем
        let snippet: String = r.get(11)?;
        Ok(EmailItem {
            id:             r.get(0)?,
            uid:            r.get::<_, i64>(1)? as u32,
            folder:         r.get(2)?,
            from_addr:      r.get(3)?,
            to_addr:        r.get(4)?,
            subject:        r.get(5)?,
            date:           r.get(6)?,
            date_ts:        r.get(7)?,
            is_read:        r.get::<_, i32>(8)? != 0,
            is_starred:     r.get::<_, i32>(9)? != 0,
            has_attachment: r.get::<_, i32>(10)? != 0,
            snippet,
        })
    }).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .collect();

    Ok(list)
}

/// Полнотекстовый поиск по всем папкам аккаунта (from, to, subject, body)
#[tauri::command]
fn search_emails(
    state: State<AppState>,
    account_id: i64,
    query: String,
    limit: Option<i64>,
) -> Result<Vec<EmailItem>, String> {
    let q = query.trim().to_string();
    if q.is_empty() { return Ok(vec![]); }
    let limit = limit.unwrap_or(200).min(500);
    // u_lower — кастомная Unicode-aware функция (поддерживает кириллицу)
    let pat = format!("%{}%", q.to_lowercase());
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, uid, folder, from_addr, to_addr, subject, date, date_ts,
                is_read, is_starred, has_attachment, snippet
         FROM emails
         WHERE account_id=?1 AND (
             u_lower(from_addr) LIKE ?2 OR
             u_lower(to_addr)   LIKE ?2 OR
             u_lower(subject)   LIKE ?2 OR
             u_lower(snippet)   LIKE ?2 OR
             u_lower(body_text) LIKE ?2
         )
         ORDER BY date_ts DESC, id DESC
         LIMIT ?3"
    ).map_err(|e| e.to_string())?;

    let list: Vec<EmailItem> = stmt.query_map(params![account_id, pat, limit], |r| {
        let snippet: String = r.get(11)?;
        Ok(EmailItem {
            id:             r.get(0)?,
            uid:            r.get::<_, i64>(1)? as u32,
            folder:         r.get(2)?,
            from_addr:      r.get(3)?,
            to_addr:        r.get(4)?,
            subject:        r.get(5)?,
            date:           r.get(6)?,
            date_ts:        r.get(7)?,
            is_read:        r.get::<_, i32>(8)? != 0,
            is_starred:     r.get::<_, i32>(9)? != 0,
            has_attachment: r.get::<_, i32>(10)? != 0,
            snippet,
        })
    }).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .collect();

    Ok(list)
}

#[tauri::command]
fn get_email_body(state: State<AppState>, email_id: i64) -> Result<EmailBody, String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT id, from_addr, to_addr, cc_addr, subject, date,
                body_html, body_text, is_read, is_starred, has_attachment,
                read_receipt_to, read_receipt_sent
         FROM emails WHERE id=?1",
        params![email_id],
        |r| Ok(EmailBody {
            id:                r.get(0)?,
            from_addr:         r.get(1)?,
            to_addr:           r.get(2)?,
            cc_addr:           r.get(3)?,
            subject:           r.get(4)?,
            date:              r.get(5)?,
            body_html:         r.get(6)?,
            body_text:         r.get(7)?,
            is_read:           r.get::<_, i32>(8)? != 0,
            is_starred:        r.get::<_, i32>(9)? != 0,
            has_attachment:    r.get::<_, i32>(10)? != 0,
            read_receipt_to:   r.get(11)?,
            read_receipt_sent: r.get::<_, i32>(12)? != 0,
        }),
    ).map_err(|e| e.to_string())
}

#[tauri::command]
fn mark_read(state: State<AppState>, email_id: i64, is_read: bool) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("UPDATE emails SET is_read=?1 WHERE id=?2",
        params![is_read as i32, email_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn toggle_starred(state: State<AppState>, email_id: i64) -> Result<bool, String> {
    let conn = state.db.lock().unwrap();
    let cur: i32 = conn.query_row(
        "SELECT is_starred FROM emails WHERE id=?1",
        params![email_id],
        |r| r.get(0),
    ).map_err(|e| e.to_string())?;
    let new_val: i32 = if cur == 0 { 1 } else { 0 };
    conn.execute("UPDATE emails SET is_starred=?1 WHERE id=?2",
        params![new_val, email_id])
        .map_err(|e| e.to_string())?;
    Ok(new_val == 1)
}

#[tauri::command]
fn move_to_trash(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("UPDATE emails SET folder='Trash' WHERE id=?1", params![email_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn move_to_spam(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("UPDATE emails SET folder='Spam' WHERE id=?1", params![email_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn move_from_spam(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("UPDATE emails SET folder='INBOX' WHERE id=?1", params![email_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn block_sender(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    let from: String = conn.query_row(
        "SELECT from_addr FROM emails WHERE id=?1", params![email_id], |r| r.get(0)
    ).map_err(|e| e.to_string())?;
    let addr = extract_email_addr(&from);
    if addr.is_empty() { return Ok(()); }
    conn.execute(
        "INSERT OR IGNORE INTO blocked_senders (email) VALUES (?1)",
        params![addr],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn unblock_sender(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    let from: String = conn.query_row(
        "SELECT from_addr FROM emails WHERE id=?1", params![email_id], |r| r.get(0)
    ).map_err(|e| e.to_string())?;
    let addr = extract_email_addr(&from);
    conn.execute(
        "DELETE FROM blocked_senders WHERE email = ?1 COLLATE NOCASE",
        params![addr],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_permanently(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    // Удаляем папку с вложениями
    let dir = attachments_dir(email_id);
    std::fs::remove_dir_all(&dir).ok();
    conn.execute("DELETE FROM attachments WHERE email_id=?1", params![email_id]).ok();
    conn.execute("DELETE FROM emails WHERE id=?1", params![email_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_data_dir_path() -> String {
    get_data_dir().to_string_lossy().to_string()
}

#[tauri::command]
fn open_data_dir() -> Result<(), String> {
    let path = get_data_dir();
    std::process::Command::new("explorer")
        .arg(path.to_string_lossy().as_ref())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn open_folder(path: String) -> Result<(), String> {
    let p = std::path::Path::new(&path);
    if !p.exists() {
        std::fs::create_dir_all(p).map_err(|e| e.to_string())?;
    }
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_log_tail(lines: usize) -> String {
    use std::io::{BufRead, BufReader};
    let path = get_data_dir().join("mail.log");
    let Ok(file) = std::fs::File::open(&path) else {
        return "Лог-файл не найден".to_string();
    };
    let reader = BufReader::new(file);
    let all: Vec<String> = reader.lines().filter_map(|l| l.ok()).collect();
    let n = lines.min(all.len());
    all[all.len() - n..].join("\n")
}

#[tauri::command]
fn clear_log() -> Result<(), String> {
    let path = get_data_dir().join("mail.log");
    std::fs::write(&path, "").map_err(|e| e.to_string())
}

/// Сбрасывает кеш синхронизации (folder_state) — при следующем запуске будет "первичная загрузка"
/// с правильной инициализацией last_uid через uid_next от сервера
#[tauri::command]
fn reset_folder_state(state: State<AppState>, account_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "DELETE FROM folder_state WHERE account_id=?1",
        params![account_id],
    ).map_err(|e| e.to_string())?;
    log_to_file(&format!("reset_folder_state: cleared for account_id={}", account_id));
    Ok(())
}

/// Сохраняет черновик. Если draft_id передан — обновляет существующий, иначе создаёт новый.
/// Возвращает id черновика.
#[tauri::command]
fn save_draft(
    state: State<AppState>,
    account_id: i64,
    draft_id: Option<i64>,
    to: String,
    cc: String,
    subject: String,
    body_text: String,
    body_html: String,
) -> Result<i64, String> {
    let conn = state.db.lock().unwrap();
    let snippet = snippet_from_text(&body_text, 100);
    let now = chrono::Local::now();
    let date_ts = now.timestamp();
    let date = now.to_rfc2822();

    if let Some(id) = draft_id {
        let updated = conn.execute(
            "UPDATE emails SET to_addr=?1, cc_addr=?2, subject=?3,
             body_text=?4, body_html=?5, snippet=?6, date=?7, date_ts=?8
             WHERE id=?9 AND account_id=?10 AND folder='Drafts'",
            params![to, cc, subject, body_text, body_html, snippet, date, date_ts, id, account_id],
        ).map_err(|e| e.to_string())?;
        if updated > 0 {
            return Ok(id);
        }
        // Черновик не найден — создаём новый
    }

    // Вставляем черновик с uid=-1, затем обновляем uid=id для уникальности
    conn.execute(
        "INSERT INTO emails
         (account_id, uid, folder, from_addr, to_addr, cc_addr, subject,
          date, date_ts, body_text, body_html, is_read, has_attachment, message_id, snippet)
         VALUES (?1, -1, 'Drafts', '', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, 0, '', ?9)",
        params![account_id, to, cc, subject, date, date_ts, body_text, body_html, snippet],
    ).map_err(|e| e.to_string())?;
    let new_id = conn.last_insert_rowid();
    // uid = id — гарантирует уникальность (UNIQUE account_id, uid, folder)
    conn.execute("UPDATE emails SET uid=?1 WHERE id=?1", params![new_id])
        .map_err(|e| e.to_string())?;
    Ok(new_id)
}

/// Удаляет черновик по id
#[tauri::command]
fn delete_draft(state: State<AppState>, draft_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "DELETE FROM emails WHERE id=?1 AND folder='Drafts'",
        params![draft_id],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

/// Количество черновиков для бейджа
#[tauri::command]
fn get_draft_count(state: State<AppState>, account_id: i64) -> Result<i64, String> {
    let conn = state.db.lock().unwrap();
    conn.query_row(
        "SELECT COUNT(*) FROM emails WHERE account_id=?1 AND folder='Drafts'",
        params![account_id],
        |r| r.get(0),
    ).map_err(|e| e.to_string())
}

/// Разбирает строку "Имя <a@b.com>, c@d.com, ..." в список Mailbox.
/// Корректно обрабатывает имена в кавычках и запятые внутри <>.
fn parse_mailbox_list(s: &str) -> Result<Vec<lettre::message::Mailbox>, String> {
    let mut result = Vec::new();
    let mut depth: usize = 0; // глубина вложенности <>
    let mut start: usize = 0;

    for (i, b) in s.bytes().enumerate() {
        match b {
            b'<' => depth += 1,
            b'>' => { if depth > 0 { depth -= 1; } }
            b',' if depth == 0 => {
                let part = s[start..i].trim();
                if !part.is_empty() {
                    result.push(parse_single_mailbox(part)?);
                }
                start = i + 1;
            }
            _ => {}
        }
    }
    let last = s[start..].trim();
    if !last.is_empty() {
        result.push(parse_single_mailbox(last)?);
    }
    Ok(result)
}

fn parse_single_mailbox(s: &str) -> Result<lettre::message::Mailbox, String> {
    if let (Some(lt), Some(gt)) = (s.find('<'), s.rfind('>')) {
        let name = s[..lt].trim().trim_matches('"').trim().to_string();
        let email_str = s[lt + 1..gt].trim();
        let addr: lettre::Address = email_str.parse()
            .map_err(|e| format!("Некорректный email '{}': {}", email_str, e))?;
        Ok(lettre::message::Mailbox::new(
            if name.is_empty() { None } else { Some(name) },
            addr,
        ))
    } else {
        let email_str = s.trim();
        let addr: lettre::Address = email_str.parse()
            .map_err(|e| format!("Некорректный email '{}': {}", email_str, e))?;
        Ok(lettre::message::Mailbox::new(None, addr))
    }
}

#[tauri::command]
async fn send_mail(state: State<'_, AppState>, req: SendMailRequest) -> Result<(), String> {
    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, req.account_id)?
    };

    // Конструируем Mailbox через Address чтобы не зависеть от парсинга строки с именем
    let from_addr: lettre::Address = account.email.parse()
        .map_err(|e| format!("Email отправителя: {}", e))?;
    let from_mailbox = lettre::message::Mailbox::new(
        Some(account.name.clone()).filter(|s| !s.is_empty()),
        from_addr,
    );

    // Поле "Кому" — поддерживаем несколько адресов через запятую
    let to_mailboxes = parse_mailbox_list(req.to.trim())
        .map_err(|e| format!("Поле «Кому»: {}", e))?;
    if to_mailboxes.is_empty() {
        return Err("Не указан получатель".to_string());
    }

    let mut builder = Message::builder().from(from_mailbox);
    for mb in to_mailboxes {
        builder = builder.to(mb);
    }
    builder = builder.subject(&req.subject);

    // Поле "Копия" — тоже поддерживаем несколько адресов
    if !req.cc.trim().is_empty() {
        let cc_mailboxes = parse_mailbox_list(req.cc.trim())
            .map_err(|e| format!("Поле «Копия»: {}", e))?;
        for mb in cc_mailboxes {
            builder = builder.cc(mb);
        }
    }

    // Запрос уведомления о прочтении (MDN, RFC 3798)
    if req.read_receipt {
        builder = builder.header(DispositionNotificationTo(account.email.clone()));
    }

    let html_body = req.body_html.clone().unwrap_or_else(|| {
        format!(
            "<pre style='font-family:sans-serif;white-space:pre-wrap'>{}</pre>",
            req.body.replace('<', "&lt;").replace('>', "&gt;")
        )
    });
    let html_body_saved = html_body.clone(); // сохраняем для записи в Sent

    let alt_part = MultiPart::alternative()
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(req.body.clone()),
        )
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_HTML)
                .body(html_body),
        );

    // Если есть вложения — оборачиваем в mixed, иначе просто alternative
    let email = if req.attachments.is_empty() {
        builder.multipart(alt_part).map_err(|e| e.to_string())?
    } else {
        let mut mixed = MultiPart::mixed().multipart(alt_part);
        for att in &req.attachments {
            let data = B64.decode(&att.data_b64)
                .unwrap_or_default();
            let ct: ContentType = att.mime_type.parse()
                .unwrap_or_else(|_| "application/octet-stream".parse().unwrap());
            mixed = mixed.singlepart(
                Attachment::new(att.filename.clone()).body(data, ct)
            );
        }
        builder.multipart(mixed).map_err(|e| e.to_string())?
    };

    let smtp_host = account.smtp_host.clone();
    let smtp_port = account.smtp_port;
    let creds = Credentials::new(account.email.clone(), account.password.clone());

    // SMTP-соединение блокирующее — выносим в отдельный поток
    tauri::async_runtime::spawn_blocking(move || {
        let mailer = SmtpTransport::relay(&smtp_host)
            .map_err(|e| e.to_string())?
            .credentials(creds)
            .port(smtp_port)
            .build();
        mailer.send(&email).map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }).await.map_err(|e| e.to_string())??;

    // Сохраняем в папку Sent локально
    {
        let conn = state.db.lock().unwrap();
        let now = chrono::Local::now().to_rfc2822();
        let date_ts = chrono::Local::now().timestamp();
        let has_attach = !req.attachments.is_empty();
        let snippet = snippet_from_text(&req.body, 100);

        // Вставляем с uid=-1, затем обновляем uid=id — гарантирует уникальность
        // (аналогично черновикам, избегает коллизии date_ts при нескольких письмах за секунду)
        conn.execute(
            "INSERT INTO emails
             (account_id, uid, folder, from_addr, to_addr, cc_addr, subject, date, date_ts,
              body_text, body_html, has_attachment, snippet, is_read)
             VALUES (?1,-1,'Sent',?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,1)",
            params![
                req.account_id,
                account.email, req.to, req.cc, req.subject, now, date_ts,
                req.body, html_body_saved, has_attach as i32, snippet
            ],
        ).ok();
        let sent_id = conn.last_insert_rowid();
        conn.execute("UPDATE emails SET uid=?1 WHERE id=?1", params![sent_id]).ok();

        // Сохраняем вложения на диск и в таблицу attachments
        if has_attach {
            let dir = attachments_dir(sent_id);
            for att in &req.attachments {
                let data = B64.decode(&att.data_b64).unwrap_or_default();
                let safe_name = if att.filename.is_empty() {
                    format!("attachment_{}.bin", chrono::Local::now().timestamp_millis())
                } else {
                    sanitize_filename(&att.filename)
                };
                let file_path = dir.join(&safe_name);
                if std::fs::write(&file_path, &data).is_ok() {
                    let path_str = file_path.to_string_lossy().to_string();
                    conn.execute(
                        "INSERT INTO attachments (email_id, filename, mime_type, file_path, file_size)
                         VALUES (?1,?2,?3,?4,?5)",
                        params![sent_id, safe_name, att.mime_type, path_str, data.len() as i64],
                    ).ok();
                }
            }
        }
    }

    Ok(())
}

/// Отправляет уведомление о прочтении (MDN) и помечает письмо как обработанное
#[tauri::command]
async fn send_mdn(
    state: State<'_, AppState>,
    email_id: i64,
    account_id: i64,
    to: String,
    original_subject: String,
) -> Result<(), String> {
    let account: Account = {
        let conn = state.db.lock().unwrap();
        get_account_by_id(&conn, account_id)?
    };

    let from_addr: lettre::Address = account.email.parse()
        .map_err(|e| format!("Email отправителя: {}", e))?;
    let from_mailbox = lettre::message::Mailbox::new(
        Some(account.name.clone()).filter(|s| !s.is_empty()), from_addr,
    );

    let to_str = to.trim();
    let to_email_str = if let Some(start) = to_str.find('<') {
        to_str[start+1..].trim_end_matches('>').trim()
    } else { to_str };
    let to_addr: lettre::Address = to_email_str.parse()
        .map_err(|e| format!("Email получателя MDN: {}", e))?;

    let mdn_subject = format!("Прочитано: {}", original_subject);
    let mdn_body = format!(
        "Это уведомление о том, что письмо \"{}\" было прочитано получателем {}.",
        original_subject, account.email
    );

    let email = Message::builder()
        .from(from_mailbox)
        .to(lettre::message::Mailbox::new(None, to_addr))
        .subject(&mdn_subject)
        .singlepart(
            SinglePart::builder()
                .header(ContentType::TEXT_PLAIN)
                .body(mdn_body),
        )
        .map_err(|e| e.to_string())?;

    let smtp_host = account.smtp_host.clone();
    let smtp_port = account.smtp_port;
    let creds = Credentials::new(account.email.clone(), account.password.clone());

    tauri::async_runtime::spawn_blocking(move || {
        let mailer = SmtpTransport::relay(&smtp_host)
            .map_err(|e| e.to_string())?
            .credentials(creds)
            .port(smtp_port)
            .build();
        mailer.send(&email).map_err(|e| e.to_string())?;
        Ok::<(), String>(())
    }).await.map_err(|e| e.to_string())??;

    // Помечаем что уведомление отправлено — баннер больше не показывать
    {
        let conn = state.db.lock().unwrap();
        conn.execute(
            "UPDATE emails SET read_receipt_sent=1 WHERE id=?1",
            params![email_id],
        ).ok();
    }
    Ok(())
}

/// Отклоняет запрос MDN — помечает как обработанный без отправки
#[tauri::command]
fn dismiss_read_receipt(state: State<AppState>, email_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute(
        "UPDATE emails SET read_receipt_sent=1 WHERE id=?1",
        params![email_id],
    ).map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_attachments(state: State<AppState>, email_id: i64) -> Result<Vec<AttachmentItem>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, email_id, filename, mime_type, file_path, file_size
         FROM attachments WHERE email_id=?1 ORDER BY id ASC"
    ).map_err(|e| e.to_string())?;

    let list: Vec<AttachmentItem> = stmt.query_map(params![email_id], |r| Ok(AttachmentItem {
        id:        r.get(0)?,
        email_id:  r.get(1)?,
        filename:  r.get(2)?,
        mime_type: r.get(3)?,
        file_path: r.get(4)?,
        file_size: r.get(5)?,
    })).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok())
    .collect();

    Ok(list)
}

#[tauri::command]
fn read_attachment_b64(file_path: String) -> Result<serde_json::Value, String> {
    let bytes = std::fs::read(&file_path).map_err(|e| e.to_string())?;
    let b64 = B64.encode(&bytes);
    let mime = match std::path::Path::new(&file_path)
        .extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str()
    {
        "pdf"  => "application/pdf",
        "jpg" | "jpeg" => "image/jpeg",
        "png"  => "image/png",
        "gif"  => "image/gif",
        "doc"  => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls"  => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "zip"  => "application/zip",
        "txt"  => "text/plain",
        _      => "application/octet-stream",
    };
    Ok(serde_json::json!({ "b64": b64, "mime_type": mime }))
}

/// Расширения файлов, опасных при открытии пользователем.
/// zip/rar/7z — стандартные архивы, остаются без изменений.
const DANGEROUS_ATTACH_EXT: &[&str] = &[
    // Исполняемые
    "exe","msi","msp","mst","com","scr","pif","dll","cpl","ocx","msc","application","xbap","appref-ms",
    // Скрипты командной строки
    "bat","cmd","ps1","psm1","psd1","ps2","ps1xml","ps2xml","psc1","psc2",
    // Скрипты Windows / интерпретируемые
    "vbs","vbe","vb","vbp","jse","js","wsf","wsh","ws","wsc","sct","shb","shs",
    // Скрипты общие (если интерпретатор установлен)
    "py","rb","pl","php",
    // HTML-приложения, ярлыки, веб-архивы
    "hta","lnk","url","website","mht","mhtml",
    // Реестр и автозапуск
    "reg","inf",
    // OneNote — массовый вектор атак 2023-2025 (встраивает vbs/exe внутрь)
    "one","onepkg",
    // SVG — может содержать JS, активно используется в фишинге 2024
    "svg",
    // Виртуальные диски — обход Mark-of-the-Web (MOTW)
    "vhd","vhdx",
    // Windows-специфичные векторы атак
    "chm","hlp","gadget","scf",
    "diagcab","diagpkg","settingcontent-ms","theme","themepack",
    "search-ms","searchconnector-ms",
    // Java
    "jar","jnlp","class",
    // Access (макросы и проекты)
    "ade","adp","mda","mdb","mde","mdt","mdw","mdz",
    // Office с макросами
    "docm","dotm","xlsm","xlsb","xltm","xlam","xll","pptm","potm","ppam","ppsm","sldm",
    "pub","pubm","wll",
    // LibreOffice / OpenOffice — макросы встраиваются в обычные форматы (CVE-2025-1080, CVE-2018-16858)
    // хакеры используют ODF специально для обхода AV, заточенных под MS Office
    "odt","ods","odp","odg","odb","ott","ots","otp","otg",
    // Архивы — опасные или редкие (zip/rar/7z оставлены без изменений)
    "iso","img","cab","arj","ace","lzh","lha",
    "lz","tar","uue","xz","z","zipx","001","bz2","gz",
];

fn is_dangerous_attach_ext(filename: &str) -> bool {
    let ext = filename.rsplit('.').next().unwrap_or("").to_lowercase();
    DANGEROUS_ATTACH_EXT.contains(&ext.as_str())
}

/// "invoice.exe" → "invoice.[virus]_exe", "doc.zip" → "doc.[virus]_zip"
fn neutralize_filename(name: &str) -> String {
    if let Some(pos) = name.rfind('.') {
        format!("{}.[virus]_{}", &name[..pos], &name[pos + 1..])
    } else {
        format!("{}.[virus]", name)
    }
}

// ── Сканирование архивов ───────────────────────────────────────────────────

#[derive(serde::Serialize, Clone, Default)]
struct ArchiveScanResult {
    /// Имена опасных файлов внутри архива
    dangerous: Vec<String>,
    /// Архив зашифрован — содержимое не проверено
    encrypted: bool,
    /// RAR, но UnRAR.exe не найден
    no_tool: bool,
}

/// Взять только имя файла из пути внутри архива (a/b/c.exe → c.exe)
fn archive_basename(name: &str) -> &str {
    name.rsplit(['/', '\\']).next().unwrap_or(name)
}

fn scan_zip(path: &str) -> ArchiveScanResult {
    let mut r = ArchiveScanResult::default();
    let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return r };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(a) => a,
        Err(_) => { r.encrypted = true; return r; }
    };
    for i in 0..archive.len() {
        match archive.by_index_raw(i) {
            Ok(entry) => {
                let name = entry.name().to_string();
                let base = archive_basename(&name);
                if !base.is_empty() && is_dangerous_attach_ext(base) {
                    r.dangerous.push(name);
                }
            }
            Err(_) => { r.encrypted = true; }
        }
    }
    r
}

fn scan_7z(path: &str) -> ArchiveScanResult {
    let mut r = ArchiveScanResult::default();
    let file = match std::fs::File::open(path) { Ok(f) => f, Err(_) => return r };
    let len = match file.metadata() { Ok(m) => m.len(), Err(_) => return r };
    let archive = match sevenz_rust::Archive::read(
        &mut std::io::BufReader::new(file), len, b""
    ) {
        Ok(a) => a,
        Err(_) => { r.encrypted = true; return r; }
    };
    for f in archive.files.iter() {
        let base = archive_basename(&f.name);
        if !base.is_empty() && is_dangerous_attach_ext(base) {
            r.dangerous.push(f.name.clone());
        }
    }
    r
}

fn find_unrar_exe() -> Option<std::path::PathBuf> {
    let candidates = [
        r"C:\Program Files\WinRAR\UnRAR.exe",
        r"C:\Program Files (x86)\WinRAR\UnRAR.exe",
    ];
    candidates.iter().map(std::path::Path::new)
        .find(|p| p.exists()).map(|p| p.to_path_buf())
}

fn scan_rar(path: &str) -> ArchiveScanResult {
    let mut r = ArchiveScanResult::default();
    let unrar = match find_unrar_exe() {
        Some(p) => p,
        None => { r.no_tool = true; return r; }
    };
    // lb = list bare (только имена файлов), -p- = без пароля
    let output = match std::process::Command::new(&unrar)
        .args(["lb", "-p-", path]).output()
    {
        Ok(o) => o,
        Err(_) => { r.no_tool = true; return r; }
    };
    if !output.status.success() {
        r.encrypted = true;
        return r;
    }
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let line = line.trim();
        if line.is_empty() { continue; }
        let base = archive_basename(line);
        if is_dangerous_attach_ext(base) {
            r.dangerous.push(line.to_string());
        }
    }
    r
}

fn scan_archive_by_ext(path: &str, ext: &str) -> ArchiveScanResult {
    match ext {
        "zip" => scan_zip(path),
        "7z"  => scan_7z(path),
        "rar" => scan_rar(path),
        _     => ArchiveScanResult::default(),
    }
}

fn build_attach_dest_dir(base: &std::path::Path, subfolder: Option<&str>) -> std::path::PathBuf {
    let now = chrono::Local::now();
    let month_ru = match now.month() {
        1  => "январь",
        2  => "февраль",
        3  => "март",
        4  => "апрель",
        5  => "май",
        6  => "июнь",
        7  => "июль",
        8  => "август",
        9  => "сентябрь",
        10 => "октябрь",
        11 => "ноябрь",
        _  => "декабрь",
    };
    let root = match subfolder {
        Some(s) if !s.is_empty() => base.join(s),
        _ => base.to_path_buf(),
    };
    root.join(now.format("%Y").to_string())
        .join(month_ru)
        .join(now.format("%d").to_string())
        .join(now.format("%H-%M").to_string())
}

#[tauri::command]
fn get_default_attach_path() -> String {
    let desktop = dirs_next::desktop_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    desktop.join("Почта").to_string_lossy().to_string()
}

#[tauri::command]
fn scan_archive(file_path: String, ext: String) -> ArchiveScanResult {
    // ext передаётся из JS по оригинальному имени файла,
    // т.к. временный file_path может не иметь расширения
    scan_archive_by_ext(&file_path, &ext.to_lowercase())
}

#[tauri::command]
fn open_attachment(
    state: State<AppState>,
    file_path: String,
    save_base: Option<String>,
    subfolder: Option<String>,
) -> Result<String, String> {
    let src = std::path::Path::new(&file_path);
    if !src.exists() {
        return Err("Файл не найден".to_string());
    }

    // Проверяем: есть ли уже сохранённый путь для этого вложения
    let saved: Option<String> = {
        let conn = state.db.lock().unwrap();
        conn.query_row(
            "SELECT saved_path FROM attachments WHERE file_path=?1",
            params![file_path],
            |r| r.get(0),
        ).ok().flatten()
    };

    let sub = subfolder.as_deref();

    let open_path = if let Some(ref sp) = saved {
        if std::path::Path::new(sp).exists() {
            sp.clone()
        } else {
            // Файл был перемещён/удалён — сохраняем заново
            let base = save_base.as_deref()
                .filter(|s| !s.is_empty())
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| {
                    dirs_next::desktop_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("Почта")
                });
            let dest_dir = build_attach_dest_dir(&base, sub);
            std::fs::create_dir_all(&dest_dir).ok();
            let fname = src.file_name().unwrap_or_default().to_string_lossy().to_string();
            let dest = dest_dir.join(&fname);
            std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
            let saved_str = dest.to_string_lossy().to_string();
            let conn = state.db.lock().unwrap();
            conn.execute("UPDATE attachments SET saved_path=?1 WHERE file_path=?2",
                params![saved_str, file_path]).ok();
            saved_str
        }
    } else {
        // Ещё не сохранён — сохраняем сейчас
        let base = save_base.as_deref()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                dirs_next::desktop_dir().unwrap_or_else(|| std::path::PathBuf::from(".")).join("Почта")
            });
        let dest_dir = build_attach_dest_dir(&base, sub);
        std::fs::create_dir_all(&dest_dir).map_err(|e| e.to_string())?;
        let fname = src.file_name().unwrap_or_default().to_string_lossy().to_string();
        let dest = dest_dir.join(&fname);
        std::fs::copy(src, &dest).map_err(|e| e.to_string())?;
        // Удаляем внутреннюю копию — она больше не нужна
        std::fs::remove_file(src).ok();
        let saved_str = dest.to_string_lossy().to_string();
        let conn = state.db.lock().unwrap();
        conn.execute("UPDATE attachments SET saved_path=?1, file_path=?1 WHERE file_path=?2",
            params![saved_str, file_path]).ok();
        saved_str
    };

    // /select,"path" — открывает папку и выделяет файл в ней.
    // raw_arg нужен чтобы кавычки вокруг пути попали в CreateProcess как есть:
    // explorer.exe /select,"C:\путь с пробелами\файл.pdf"
    // Если использовать .arg() — Windows обернёт весь аргумент и explorer
    // получит "/select,path" вместо /select,"path", после чего откроет Документы.
    use std::os::windows::process::CommandExt;
    std::process::Command::new("explorer")
        .raw_arg(format!("/select,\"{}\"", open_path))
        .spawn()
        .map_err(|e| e.to_string())?;

    Ok(open_path)
}

#[tauri::command]
fn clear_trash(state: State<AppState>, account_id: i64) -> Result<usize, String> {
    let conn = state.db.lock().unwrap();
    // Получаем id всех писем в корзине для удаления вложений
    let mut stmt = conn.prepare(
        "SELECT id FROM emails WHERE account_id=?1 AND folder='Trash'"
    ).map_err(|e| e.to_string())?;
    let ids: Vec<i64> = stmt.query_map(params![account_id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    for id in &ids {
        conn.execute("DELETE FROM attachments WHERE email_id=?1", params![id]).ok();
        let dir = attachments_dir(*id);
        std::fs::remove_dir_all(&dir).ok();
    }

    let count = conn.execute(
        "DELETE FROM emails WHERE account_id=?1 AND folder='Trash'",
        params![account_id],
    ).map_err(|e| e.to_string())?;

    Ok(count)
}

#[tauri::command]
fn clear_spam(state: State<AppState>, account_id: i64) -> Result<usize, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id FROM emails WHERE account_id=?1 AND folder='Spam'"
    ).map_err(|e| e.to_string())?;
    let ids: Vec<i64> = stmt.query_map(params![account_id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok())
        .collect();

    for id in &ids {
        conn.execute("DELETE FROM attachments WHERE email_id=?1", params![id]).ok();
        let dir = attachments_dir(*id);
        std::fs::remove_dir_all(&dir).ok();
    }

    let count = conn.execute(
        "DELETE FROM emails WHERE account_id=?1 AND folder='Spam'",
        params![account_id],
    ).map_err(|e| e.to_string())?;

    Ok(count)
}

#[tauri::command]
fn delete_emails_bulk(
    state: State<AppState>,
    email_ids: Vec<i64>,
    permanently: bool,
) -> Result<usize, String> {
    let conn = state.db.lock().unwrap();
    let mut count = 0usize;
    for id in &email_ids {
        if permanently {
            conn.execute("DELETE FROM attachments WHERE email_id=?1", params![id]).ok();
            if conn.execute("DELETE FROM emails WHERE id=?1", params![id])
                .map(|n| n > 0).unwrap_or(false) { count += 1; }
        } else {
            if conn.execute("UPDATE emails SET folder='Trash' WHERE id=?1", params![id])
                .map(|n| n > 0).unwrap_or(false) { count += 1; }
        }
    }
    Ok(count)
}

#[tauri::command]
fn mark_read_bulk(state: State<AppState>, email_ids: Vec<i64>, is_read: bool) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    for id in &email_ids {
        conn.execute("UPDATE emails SET is_read=?1 WHERE id=?2", params![is_read as i32, id])
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn mark_all_read(state: State<AppState>, account_id: i64, folder: String) -> Result<u32, String> {
    let conn = state.db.lock().unwrap();
    let count = conn.execute(
        "UPDATE emails SET is_read=1 WHERE account_id=?1 AND folder=?2 AND is_read=0",
        params![account_id, folder],
    ).map_err(|e| e.to_string())? as u32;
    Ok(count)
}

// ─── Контакты ─────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, Clone)]
struct Contact {
    id:             i64,
    name:           String,
    email:          String,
    phone:          String,
    company:        String,
    notes:          String,
    #[serde(default)]
    is_blacklisted: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
struct ContactGroup {
    id:   i64,
    name: String,
}

fn parse_vcf_content(content: &str) -> Vec<(String,String,String,String,String)> {
    let mut result = Vec::new();
    let mut in_card = false;
    let mut name = String::new();
    let mut email = String::new();
    let mut phone = String::new();
    let mut company = String::new();
    let mut notes = String::new();

    for line in content.lines() {
        let line = line.trim();
        if line.eq_ignore_ascii_case("BEGIN:VCARD") {
            in_card = true;
            name.clear(); email.clear(); phone.clear(); company.clear(); notes.clear();
        } else if line.eq_ignore_ascii_case("END:VCARD") && in_card {
            if !name.is_empty() || !email.is_empty() {
                result.push((name.clone(), email.clone(), phone.clone(), company.clone(), notes.clone()));
            }
            in_card = false;
        } else if in_card {
            if let Some(rest) = line.strip_prefix("FN:") {
                if name.is_empty() { name = rest.trim().to_string(); }
            } else if line.starts_with("N:") && name.is_empty() {
                let parts: Vec<&str> = line[2..].splitn(5, ';').collect();
                let last = parts.first().unwrap_or(&"").trim();
                let first = parts.get(1).unwrap_or(&"").trim();
                let n = format!("{} {}", first, last).trim().to_string();
                if !n.is_empty() { name = n; }
            } else if line.starts_with("EMAIL") {
                if let Some(pos) = line.find(':') {
                    if email.is_empty() { email = line[pos+1..].trim().to_string(); }
                }
            } else if line.starts_with("TEL") {
                if let Some(pos) = line.find(':') {
                    if phone.is_empty() { phone = line[pos+1..].trim().to_string(); }
                }
            } else if line.starts_with("ORG:") {
                if company.is_empty() {
                    company = line[4..].split(';').next().unwrap_or("").trim().to_string();
                }
            } else if line.starts_with("NOTE:") {
                if notes.is_empty() { notes = line[5..].trim().to_string(); }
            }
        }
    }
    result
}

fn split_csv_row(line: &str) -> Vec<String> {
    let mut cols = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    for c in line.chars() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => { cols.push(cur.trim().to_string()); cur.clear(); }
            _ => cur.push(c),
        }
    }
    cols.push(cur.trim().to_string());
    cols
}

fn parse_csv_contacts(content: &str) -> Vec<(String,String,String,String,String)> {
    let mut lines = content.lines();
    let header_line = match lines.next() { Some(h) => h, None => return Vec::new() };
    let headers: Vec<String> = split_csv_row(header_line).iter().map(|s| s.to_lowercase()).collect();

    let col = |names: &[&str]| -> Option<usize> {
        headers.iter().position(|h| names.iter().any(|n| h.contains(n)))
    };

    let name_col    = col(&["name","имя","full"]);
    let first_col   = col(&["first","given"]);
    let last_col    = col(&["last","family","surname"]);
    let email_col   = col(&["email","e-mail","почта"]);
    let phone_col   = col(&["phone","tel","mobile","телефон"]);
    let company_col = col(&["company","org","компания"]);
    let notes_col   = col(&["note","заметк"]);

    let mut result = Vec::new();
    for line in lines {
        if line.trim().is_empty() { continue; }
        let cols = split_csv_row(line);
        let get = |idx: Option<usize>| -> String {
            idx.and_then(|i| cols.get(i)).map(|s| s.clone()).unwrap_or_default()
        };
        let name = if name_col.is_some() {
            get(name_col)
        } else {
            format!("{} {}", get(first_col), get(last_col)).trim().to_string()
        };
        let email = get(email_col);
        if name.is_empty() && email.is_empty() { continue; }
        result.push((name, email, get(phone_col), get(company_col), get(notes_col)));
    }
    result
}

fn insert_contacts(conn: &Connection, list: &[(String,String,String,String,String)], group_id: Option<i64>) -> usize {
    let mut count = 0;
    for (name, email, phone, company, notes) in list {
        if email.is_empty() && name.is_empty() { continue; }
        conn.execute(
            "INSERT INTO contacts (name,email,phone,company,notes) VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(email) DO UPDATE SET
               name=CASE WHEN excluded.name!='' THEN excluded.name ELSE name END,
               phone=CASE WHEN excluded.phone!='' THEN excluded.phone ELSE phone END,
               company=CASE WHEN excluded.company!='' THEN excluded.company ELSE company END",
            params![name, email, phone, company, notes],
        ).ok();
        if let Ok(id) = conn.query_row(
            "SELECT id FROM contacts WHERE email=?1", params![email], |r| r.get::<_,i64>(0)
        ) {
            add_to_default_group(conn, id);
            if let Some(gid) = group_id {
                conn.execute(
                    "INSERT OR IGNORE INTO contact_group_members (contact_id, group_id) VALUES (?1, ?2)",
                    params![id, gid],
                ).ok();
            }
            count += 1;
        }
    }
    count
}

#[tauri::command]
fn get_contacts(state: State<AppState>) -> Result<Vec<Contact>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id,name,email,phone,company,notes,is_blacklisted FROM contacts ORDER BY name COLLATE NOCASE ASC"
    ).map_err(|e| e.to_string())?;
    let list: Vec<Contact> = stmt.query_map([], |r| Ok(Contact {
        id:             r.get(0)?,
        name:           r.get(1)?,
        email:          r.get(2)?,
        phone:          r.get(3)?,
        company:        r.get(4)?,
        notes:          r.get(5)?,
        is_blacklisted: r.get::<_, i32>(6)? != 0,
    })).map_err(|e| e.to_string())?
    .filter_map(|r| r.ok()).collect();
    Ok(list)
}

#[tauri::command]
fn save_contact(state: State<AppState>, contact: Contact) -> Result<Contact, String> {
    let conn = state.db.lock().unwrap();
    let id = if contact.id > 0 {
        conn.execute(
            "UPDATE contacts SET name=?1,email=?2,phone=?3,company=?4,notes=?5 WHERE id=?6",
            params![contact.name, contact.email, contact.phone, contact.company, contact.notes, contact.id],
        ).map_err(|e| e.to_string())?;
        contact.id
    } else {
        conn.execute(
            "INSERT INTO contacts (name,email,phone,company,notes) VALUES (?1,?2,?3,?4,?5)",
            params![contact.name, contact.email, contact.phone, contact.company, contact.notes],
        ).map_err(|e| e.to_string())?;
        conn.last_insert_rowid()
    };
    add_to_default_group(&conn, id);
    Ok(Contact { id, name: contact.name, email: contact.email, phone: contact.phone, company: contact.company, notes: contact.notes, is_blacklisted: contact.is_blacklisted })
}

#[tauri::command]
fn delete_contact(state: State<AppState>, id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("DELETE FROM contacts WHERE id=?1", params![id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn import_contacts_vcf(state: State<AppState>, content: String, group_id: Option<i64>) -> Result<usize, String> {
    let list = parse_vcf_content(&content);
    let conn = state.db.lock().unwrap();
    Ok(insert_contacts(&conn, &list, group_id))
}

#[tauri::command]
fn import_contacts_csv(state: State<AppState>, content: String, group_id: Option<i64>) -> Result<usize, String> {
    let list = parse_csv_contacts(&content);
    let conn = state.db.lock().unwrap();
    Ok(insert_contacts(&conn, &list, group_id))
}

#[tauri::command]
fn add_contact_from_email(state: State<AppState>, email_id: i64) -> Result<Contact, String> {
    let conn = state.db.lock().unwrap();
    let from_addr: String = conn.query_row(
        "SELECT from_addr FROM emails WHERE id=?1", params![email_id], |r| r.get(0)
    ).map_err(|e| e.to_string())?;

    // Разбираем "Имя <email>" или просто "email"
    let (name, email) = if let Some(start) = from_addr.find('<') {
        let n = from_addr[..start].trim().trim_matches('"').to_string();
        let e = from_addr[start+1..].trim_end_matches('>').trim().to_string();
        (n, e)
    } else {
        (String::new(), from_addr.trim().to_string())
    };

    if email.is_empty() { return Err("Не удалось извлечь email".to_string()); }

    conn.execute(
        "INSERT INTO contacts (name,email,phone,company,notes) VALUES (?1,?2,'','','')
         ON CONFLICT(email) DO UPDATE SET name=CASE WHEN excluded.name!='' THEN excluded.name ELSE name END",
        params![name, email],
    ).map_err(|e| e.to_string())?;

    let id: i64 = conn.query_row(
        "SELECT id FROM contacts WHERE email=?1", params![email], |r| r.get(0)
    ).map_err(|e| e.to_string())?;

    add_to_default_group(&conn, id);
    Ok(Contact { id, name, email, phone: String::new(), company: String::new(), notes: String::new(), is_blacklisted: false })
}

#[tauri::command]
fn get_groups(state: State<AppState>) -> Result<Vec<ContactGroup>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT id, name FROM contact_groups ORDER BY name"
    ).map_err(|e| e.to_string())?;
    let list = stmt.query_map([], |r| Ok(ContactGroup { id: r.get(0)?, name: r.get(1)? }))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    Ok(list)
}

#[tauri::command]
fn save_group(state: State<AppState>, name: String) -> Result<ContactGroup, String> {
    let name = name.trim().to_string();
    if name.is_empty() { return Err("Название не может быть пустым".to_string()); }
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO contact_groups (name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
        params![name],
    ).map_err(|e| e.to_string())?;
    let id: i64 = conn.query_row(
        "SELECT id FROM contact_groups WHERE name=?1", params![name], |r| r.get(0)
    ).map_err(|e| e.to_string())?;
    Ok(ContactGroup { id, name })
}

#[tauri::command]
fn rename_group(state: State<AppState>, group_id: i64, name: String) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() { return Err("Название не может быть пустым".to_string()); }
    let conn = state.db.lock().unwrap();
    conn.execute("UPDATE contact_groups SET name=?1 WHERE id=?2", params![name, group_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn delete_group(state: State<AppState>, group_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("DELETE FROM contact_group_members WHERE group_id=?1", params![group_id])
        .map_err(|e| e.to_string())?;
    conn.execute("DELETE FROM contact_groups WHERE id=?1", params![group_id])
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
fn get_contact_groups(state: State<AppState>, contact_id: i64) -> Result<Vec<i64>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT group_id FROM contact_group_members WHERE contact_id=?1"
    ).map_err(|e| e.to_string())?;
    let ids = stmt.query_map(params![contact_id], |r| r.get(0))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    Ok(ids)
}

#[tauri::command]
fn set_contact_groups(state: State<AppState>, contact_id: i64, group_ids: Vec<i64>) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    conn.execute("DELETE FROM contact_group_members WHERE contact_id=?1", params![contact_id])
        .map_err(|e| e.to_string())?;
    for gid in group_ids {
        conn.execute(
            "INSERT OR IGNORE INTO contact_group_members (contact_id, group_id) VALUES (?1, ?2)",
            params![contact_id, gid],
        ).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn add_contacts_to_group(state: State<AppState>, contact_ids: Vec<i64>, group_id: i64) -> Result<(), String> {
    let conn = state.db.lock().unwrap();
    for cid in contact_ids {
        conn.execute(
            "INSERT OR IGNORE INTO contact_group_members (contact_id, group_id) VALUES (?1, ?2)",
            params![cid, group_id],
        ).map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tauri::command]
fn get_contacts_by_group(state: State<AppState>, group_id: i64) -> Result<Vec<Contact>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT c.id, c.name, c.email, c.phone, c.company, c.notes, c.is_blacklisted
         FROM contacts c
         JOIN contact_group_members m ON c.id = m.contact_id
         WHERE m.group_id = ?1
         ORDER BY c.name, c.email"
    ).map_err(|e| e.to_string())?;
    let list = stmt.query_map(params![group_id], |r| Ok(Contact {
        id: r.get(0)?, name: r.get(1)?, email: r.get(2)?,
        phone: r.get(3)?, company: r.get(4)?, notes: r.get(5)?,
        is_blacklisted: r.get::<_, i32>(6)? != 0,
    })).map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    Ok(list)
}

/// Добавляет адрес в контакты (если нет) и ставит is_blacklisted=1
#[tauri::command]
fn blacklist_sender(state: State<AppState>, from_addr: String) -> Result<(), String> {
    let (name, email) = if let Some(start) = from_addr.find('<') {
        let n = from_addr[..start].trim().trim_matches('"').to_string();
        let e = from_addr[start+1..].trim_end_matches('>').trim().to_string();
        (n, e)
    } else {
        (String::new(), from_addr.trim().to_string())
    };
    if email.is_empty() { return Ok(()); }
    let conn = state.db.lock().unwrap();
    conn.execute(
        "INSERT INTO contacts (name,email,phone,company,notes,is_blacklisted) VALUES (?1,?2,'','','',1)
         ON CONFLICT(email) DO UPDATE SET is_blacklisted=1,
         name=CASE WHEN excluded.name!='' THEN excluded.name ELSE name END",
        params![name, email],
    ).map_err(|e| e.to_string())?;
    // Перемещаем уже существующие письма от этого адреса в Спам
    conn.execute(
        "UPDATE emails SET folder='Spam'
         WHERE folder!='Spam' AND INSTR(LOWER(from_addr), LOWER(?1)) > 0",
        params![email],
    ).ok();
    Ok(())
}

#[tauri::command]
fn toggle_contact_blacklist(state: State<AppState>, id: i64) -> Result<bool, String> {
    let conn = state.db.lock().unwrap();
    let (current, email): (i32, String) = conn.query_row(
        "SELECT is_blacklisted, email FROM contacts WHERE id=?1", params![id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    ).map_err(|e| e.to_string())?;
    let new_val = if current == 0 { 1i32 } else { 0i32 };
    conn.execute("UPDATE contacts SET is_blacklisted=?1 WHERE id=?2", params![new_val, id])
        .map_err(|e| e.to_string())?;
    // При блокировке — перемещаем существующие письма в Спам
    if new_val == 1 && !email.is_empty() {
        conn.execute(
            "UPDATE emails SET folder='Spam'
             WHERE folder!='Spam' AND INSTR(LOWER(from_addr), LOWER(?1)) > 0",
            params![email],
        ).ok();
    }
    Ok(new_val != 0)
}

#[tauri::command]
fn get_unread_count(state: State<AppState>, account_id: i64) -> Result<i64, String> {
    let conn = state.db.lock().unwrap();
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM emails WHERE account_id=?1 AND folder='INBOX' AND is_read=0",
        params![account_id],
        |r| r.get(0),
    ).unwrap_or(0);
    Ok(count)
}

#[derive(Serialize)]
struct AccountUnread { account_id: i64, count: i64 }

#[tauri::command]
fn get_all_unread_counts(state: State<AppState>) -> Result<Vec<AccountUnread>, String> {
    let conn = state.db.lock().unwrap();
    let mut stmt = conn.prepare(
        "SELECT account_id, COUNT(*) FROM emails WHERE folder='INBOX' AND is_read=0 GROUP BY account_id"
    ).map_err(|e| e.to_string())?;
    let rows = stmt.query_map([], |r| Ok(AccountUnread { account_id: r.get(0)?, count: r.get(1)? }))
        .map_err(|e| e.to_string())?
        .filter_map(|r| r.ok()).collect();
    Ok(rows)
}

// ─── Автозапуск (реестр Windows) ─────────────────────────────────────────────

const AUTOSTART_REG_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const AUTOSTART_VALUE: &str = "ДокВис Почта";

#[tauri::command]
fn get_autostart() -> bool {
    std::process::Command::new("reg")
        .args(["query",
               &format!("HKCU\\{}", AUTOSTART_REG_KEY),
               "/v", AUTOSTART_VALUE])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[tauri::command]
fn set_autostart(enable: bool) -> Result<(), String> {
    if enable {
        // Путь к исполняемому файлу текущего процесса
        let exe = std::env::current_exe()
            .map_err(|e| e.to_string())?
            .to_string_lossy()
            .to_string();
        let value = format!("\"{}\" --minimized", exe);
        let status = std::process::Command::new("reg")
            .args(["add",
                   &format!("HKCU\\{}", AUTOSTART_REG_KEY),
                   "/v", AUTOSTART_VALUE,
                   "/t", "REG_SZ",
                   "/d", &value,
                   "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() { return Err("reg add failed".to_string()); }
    } else {
        std::process::Command::new("reg")
            .args(["delete",
                   &format!("HKCU\\{}", AUTOSTART_REG_KEY),
                   "/v", AUTOSTART_VALUE, "/f"])
            .creation_flags(CREATE_NO_WINDOW)
            .output().ok(); // ok если значения нет — ошибку игнорируем
    }
    Ok(())
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() {
    rotate_log_if_needed();

    // Если есть отложенное восстановление — применяем до открытия БД
    let pending = get_data_dir().join("pending_restore.db");
    if pending.exists() {
        let db_path = get_db_path();
        if let Err(e) = std::fs::copy(&pending, &db_path) {
            eprintln!("Restore failed: {}", e);
        } else {
            let _ = std::fs::remove_file(&pending);
            log_to_file("Database restored from backup.");
        }
    }

    let db_path = get_db_path();
    let conn = Connection::open(&db_path).expect("Cannot open DB");
    conn.execute_batch("
        PRAGMA journal_mode=WAL;
        PRAGMA synchronous=NORMAL;
        PRAGMA cache_size=-16000;
        PRAGMA temp_store=MEMORY;
        PRAGMA mmap_size=67108864;
    ").ok();
    // Кастомная функция u_lower — Unicode-aware lowercase (поддерживает кириллицу)
    conn.create_scalar_function(
        "u_lower", 1,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8 | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        |ctx| {
            let text: Option<String> = ctx.get(0)?;
            Ok(text.map(|s| s.to_lowercase()))
        },
    ).ok();

    init_db(&conn);

    let start_minimized = std::env::args().any(|a| a == "--minimized");

    // ── Меню трея ────────────────────────────────────────────────────────────
    let tray_menu = SystemTrayMenu::new()
        .add_item(CustomMenuItem::new("open", "Открыть"))
        .add_item(CustomMenuItem::new("check_mail", "Проверить почту"))
        .add_native_item(tauri::SystemTrayMenuItem::Separator)
        .add_item(CustomMenuItem::new("quit", "Закрыть"));

    let tray = SystemTray::new()
        .with_menu(tray_menu)
        .with_tooltip("ДокВис Почта");

    tauri::Builder::default()
        .system_tray(tray)
        .on_system_tray_event(|app, event| match event {
            // Левый клик — показываем окно
            SystemTrayEvent::LeftClick { .. } => {
                if let Some(w) = app.get_window("main") {
                    let _ = w.show();
                    let _ = w.unminimize();
                    let _ = w.set_focus();
                }
            }
            SystemTrayEvent::MenuItemClick { id, .. } => match id.as_str() {
                "open" => {
                    if let Some(w) = app.get_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                    }
                }
                "check_mail" => {
                    if let Some(w) = app.get_window("main") {
                        let _ = w.show();
                        let _ = w.unminimize();
                        let _ = w.set_focus();
                    }
                    app.emit_all("tray-check-mail", ()).ok();
                }
                "quit" => std::process::exit(0),
                _ => {}
            },
            _ => {}
        })
        // Закрытие окна → скрываем в трей (не завершаем)
        .on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event.event() {
                api.prevent_close();
                let _ = event.window().hide();
            }
        })
        .setup(move |app| {
            // Устанавливаем иконку окна из PNG (256×256 → Windows масштабирует вниз, что даёт чёткость)
            if let Some(w) = app.get_window("main") {
                let png_bytes = include_bytes!("../icons/icon.png");
                if let Ok(img) = image::load_from_memory_with_format(png_bytes, image::ImageFormat::Png) {
                    let rgba = img.into_rgba8();
                    let (width, height) = (rgba.width(), rgba.height());
                    let _ = w.set_icon(tauri::Icon::Rgba { rgba: rgba.into_raw(), width, height });
                }
            }

            // Запуск с --minimized: окно сразу скрываем
            if start_minimized {
                if let Some(w) = app.get_window("main") {
                    let _ = w.hide();
                }
            }

            // ── Фоновый поток автоматического резервного копирования ─────────
            let app_handle = app.handle();
            std::thread::spawn(move || {
                loop {
                    // Проверяем раз в час
                    std::thread::sleep(std::time::Duration::from_secs(3600));
                    let settings = read_backup_settings();
                    if !settings.enabled { continue; }

                    let should_backup = match &settings.last_backup {
                        None => true,
                        Some(ts) => {
                            if let Ok(last) = chrono::DateTime::parse_from_rfc3339(ts) {
                                let elapsed = chrono::Local::now()
                                    .signed_duration_since(last.with_timezone(&chrono::Local));
                                elapsed.num_days() >= settings.interval_days as i64
                            } else { true }
                        }
                    };

                    if should_backup {
                        let state: tauri::State<AppState> = app_handle.state();
                        if let Ok(conn) = state.db.lock() {
                            let _ = conn.execute_batch("PRAGMA wal_checkpoint(FULL)");
                        }
                        match do_backup_internal() {
                            Ok(path) => log_to_file(&format!("Auto-backup: {}", path)),
                            Err(e)   => log_to_file(&format!("Auto-backup failed: {}", e)),
                        }
                    }
                }
            });

            Ok(())
        })
        .manage(AppState { db: Mutex::new(conn) })
        .invoke_handler(tauri::generate_handler![
            get_accounts,
            save_account,
            update_account,
            delete_account,
            sync_folder,
            full_sync_folder,
            cancel_full_sync,
            get_emails,
            search_emails,
            get_email_body,
            mark_read,
            toggle_starred,
            move_to_trash,
            move_to_spam,
            move_from_spam,
            block_sender,
            unblock_sender,
            get_data_dir_path,
            open_data_dir,
            open_folder,
            get_log_tail,
            clear_log,
            reset_folder_state,
            save_draft,
            delete_draft,
            get_draft_count,
            get_cached_folders,
            list_imap_folders,
            create_imap_folder,
            delete_imap_folder,
            rename_imap_folder,
            move_emails_to_folder,
            delete_permanently,
            clear_trash,
            clear_spam,
            delete_emails_bulk,
            mark_read_bulk,
            mark_all_read,
            send_mail,
            send_mdn,
            dismiss_read_receipt,
            get_unread_count,
            get_all_unread_counts,
            get_attachments,
            open_attachment,
            get_default_attach_path,
            auto_save_attachments,
            read_attachment_b64,
            get_contacts,
            save_contact,
            delete_contact,
            import_contacts_vcf,
            import_contacts_csv,
            add_contact_from_email,
            get_groups,
            save_group,
            rename_group,
            delete_group,
            get_contact_groups,
            set_contact_groups,
            get_contacts_by_group,
            add_contacts_to_group,
            toggle_contact_blacklist,
            blacklist_sender,
            prewarm_powershell,
            show_mail_notification,
            get_autostart,
            set_autostart,
            backup_database,
            restore_database,
            get_backup_settings,
            set_backup_settings,
            get_backups_dir_path,
            open_backups_folder,
            restart_app,
            scan_archive,
        ])
        .run(tauri::generate_context!())
        .expect("error while running application");
}
