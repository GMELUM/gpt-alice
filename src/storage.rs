//! SQLite stores only authenticated ciphertext. The device key never enters SQLite.
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, OsRng, Payload, rand_core::RngCore},
};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Serialize, de::DeserializeOwned};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use zeroize::Zeroizing;

#[derive(Clone)]
pub struct Storage(Arc<Inner>);
struct Inner {
    connection: Mutex<Connection>,
    cipher: XChaCha20Poly1305,
}

impl Storage {
    pub fn open(directory: &Path) -> Result<Self, String> {
        fs::create_dir_all(directory).map_err(|_| "Не удалось создать каталог хранилища")?;
        let database = directory.join("state.sqlite");
        let key_path = directory.join("storage.key");
        let key = match fs::read(&key_path) {
            Ok(bytes) => Zeroizing::new(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if database.exists() {
                    return Err(
                        "Отсутствует storage.key для существующей БД; восстановите исходный ключ"
                            .into(),
                    );
                }
                let mut bytes = Zeroizing::new(vec![0u8; 32]);
                OsRng
                    .try_fill_bytes(&mut bytes)
                    .map_err(|_| "Недоступен системный генератор случайных чисел")?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&key_path)
                    .map_err(|_| "Не удалось создать ключ хранилища")?;
                file.write_all(&bytes)
                    .and_then(|()| file.sync_all())
                    .map_err(|_| "Не удалось сохранить ключ хранилища")?;
                fs::File::open(directory)
                    .and_then(|f| f.sync_all())
                    .map_err(|_| "Не удалось синхронизировать каталог ключа")?;
                bytes
            }
            Err(_) => return Err("Не удалось прочитать ключ хранилища".into()),
        };
        if key.len() != 32 {
            return Err("Некорректный ключ хранилища".into());
        }
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Не удалось закрыть права ключа")?;
        OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&database)
            .map_err(|_| "Не удалось открыть БД")?;
        fs::set_permissions(&database, fs::Permissions::from_mode(0o600))
            .map_err(|_| "Не удалось закрыть права БД")?;
        let connection = Connection::open(database).map_err(db_error)?;
        Self::initialize(connection, &key)
    }

    fn initialize(connection: Connection, key: &[u8]) -> Result<Self, String> {
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(db_error)?;
        let version: u32 = connection
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(db_error)?;
        if version > 1 {
            return Err("Версия БД новее этой прошивки".into());
        }
        connection
            .execute_batch(
                "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA secure_delete=ON;
            CREATE TABLE IF NOT EXISTS records (name TEXT PRIMARY KEY, value BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY);
            PRAGMA user_version=1;",
            )
            .map_err(db_error)?;
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| "Некорректный ключ")?;
        let store = Self(Arc::new(Inner {
            connection: Mutex::new(connection),
            cipher,
        }));
        // Authenticate every existing row before any migration or device changes.
        {
            let connection = store
                .0
                .connection
                .lock()
                .map_err(|_| "Хранилище недоступно")?;
            let mut statement = connection
                .prepare("SELECT name, value FROM records")
                .map_err(db_error)?;
            let rows = statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
                })
                .map_err(db_error)?;
            for row in rows {
                let (name, value) = row.map_err(db_error)?;
                store.decrypt(&name, &value)?;
            }
        }
        if store.get::<String>("key_check")?.is_none() {
            store.set("key_check", &"gpt-alice-storage-v1")?;
        }
        Ok(store)
    }

    #[cfg(test)]
    pub fn memory() -> Self {
        Self::initialize(Connection::open_in_memory().unwrap(), &[7; 32]).unwrap()
    }

    fn encrypt(&self, name: &str, bytes: &[u8]) -> Result<Vec<u8>, String> {
        let mut nonce = [0u8; 24];
        OsRng
            .try_fill_bytes(&mut nonce)
            .map_err(|_| "Недоступен генератор случайных чисел")?;
        let aad = format!("gpt-alice:v1:{name}");
        let encrypted = self
            .0
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: bytes,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| "Не удалось зашифровать данные")?;
        let mut result = Vec::with_capacity(25 + encrypted.len());
        result.push(1);
        result.extend(nonce);
        result.extend(encrypted);
        Ok(result)
    }

    fn decrypt(&self, name: &str, value: &[u8]) -> Result<Zeroizing<Vec<u8>>, String> {
        if value.len() < 41 || value[0] != 1 {
            return Err("Повреждён формат зашифрованной записи".into());
        }
        let aad = format!("gpt-alice:v1:{name}");
        self.0
            .cipher
            .decrypt(
                XNonce::from_slice(&value[1..25]),
                Payload {
                    msg: &value[25..],
                    aad: aad.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| "Не удалось расшифровать БД: неверный ключ или повреждённые данные".into())
    }

    pub fn get<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>, String> {
        let connection = self
            .0
            .connection
            .lock()
            .map_err(|_| "Хранилище недоступно")?;
        let value: Option<Vec<u8>> = connection
            .query_row("SELECT value FROM records WHERE name=?1", [name], |row| {
                row.get(0)
            })
            .optional()
            .map_err(db_error)?;
        value
            .map(|bytes| {
                serde_json::from_slice(&self.decrypt(name, &bytes)?)
                    .map_err(|_| format!("Некорректные данные раздела {name}"))
            })
            .transpose()
    }

    pub fn set<T: Serialize>(&self, name: &str, data: &T) -> Result<(), String> {
        let plaintext = Zeroizing::new(
            serde_json::to_vec(data).map_err(|_| "Не удалось сериализовать данные")?,
        );
        let ciphertext = self.encrypt(name, &plaintext)?;
        self.0.connection.lock().map_err(|_| "Хранилище недоступно")?
            .execute("INSERT INTO records(name,value) VALUES (?1,?2) ON CONFLICT(name) DO UPDATE SET value=excluded.value", params![name, ciphertext]).map_err(db_error)?;
        Ok(())
    }

    pub fn delete(&self, name: &str) -> Result<(), String> {
        self.0
            .connection
            .lock()
            .map_err(|_| "Хранилище недоступно")?
            .execute("DELETE FROM records WHERE name=?1", [name])
            .map_err(db_error)?;
        Ok(())
    }

    /// Crash-safe, one-time import. A committed marker prevents stale files from
    /// restoring credentials after logout. Files are removed only after commit.
    pub fn import_json<T: DeserializeOwned + Serialize>(
        &self,
        name: &str,
        path: &Path,
    ) -> Result<(), String> {
        self.import(name, path, true, |bytes| {
            let data: T = serde_json::from_slice(bytes).map_err(|_| {
                format!("Некорректный старый файл раздела {name}; миграция остановлена")
            })?;
            serde_json::to_vec(&data).map_err(|_| "Ошибка сериализации миграции".into())
        })
    }

    pub fn import_text(&self, name: &str, path: &Path, remove: bool) -> Result<(), String> {
        // AWG still needs a protected plaintext export for its system init script.
        self.import(name, path, remove, |bytes| {
            let text = std::str::from_utf8(bytes).map_err(|_| "Некорректный текст конфигурации")?;
            serde_json::to_vec(text).map_err(|_| "Ошибка сериализации конфигурации".into())
        })
    }

    fn import(
        &self,
        name: &str,
        path: &Path,
        remove: bool,
        decode: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
    ) -> Result<(), String> {
        let mut connection = self
            .0
            .connection
            .lock()
            .map_err(|_| "Хранилище недоступно")?;
        let transaction = connection.transaction().map_err(db_error)?;
        let imported: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM migrations WHERE name=?1)",
                [name],
                |r| r.get(0),
            )
            .map_err(db_error)?;
        if !imported {
            match fs::read(path) {
                Ok(bytes) => {
                    let bytes = Zeroizing::new(bytes);
                    let plaintext = Zeroizing::new(decode(&bytes)?);
                    let ciphertext = self.encrypt(name, &plaintext)?;
                    transaction
                        .execute(
                            "INSERT OR IGNORE INTO records(name,value) VALUES (?1,?2)",
                            params![name, ciphertext],
                        )
                        .map_err(db_error)?;
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(format!("Не удалось прочитать старый файл раздела {name}")),
            }
            transaction
                .execute("INSERT INTO migrations(name) VALUES (?1)", [name])
                .map_err(db_error)?;
        }
        transaction.commit().map_err(db_error)?;
        if remove {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    return Err(format!(
                        "Данные {name} перенесены, но старый файл не удалось удалить"
                    ));
                }
            }
        }
        Ok(())
    }
}

fn db_error(_: rusqlite::Error) -> String {
    "Ошибка SQLite: проверьте доступ к БД и свободное место".into()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!("gpt-alice-storage-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn migration_encrypts_survives_restart_and_does_not_resurrect_logout() {
        let dir = directory();
        let legacy = dir.join("auth.json");
        let secret = "test-secret-never-in-sqlite-plaintext";
        fs::write(&legacy, serde_json::to_vec(&secret).unwrap()).unwrap();
        let db = Storage::open(&dir).unwrap();
        db.import_json::<String>("oauth", &legacy).unwrap();
        assert!(!legacy.exists());
        assert_eq!(db.get::<String>("oauth").unwrap().unwrap(), secret);
        drop(db);
        assert!(
            !fs::read(dir.join("state.sqlite"))
                .unwrap()
                .windows(secret.len())
                .any(|b| b == secret.as_bytes())
        );
        assert_eq!(
            fs::metadata(dir.join("storage.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let db = Storage::open(&dir).unwrap();
        assert_eq!(db.get::<String>("oauth").unwrap().unwrap(), secret);
        db.delete("oauth").unwrap();
        fs::write(&legacy, serde_json::to_vec(&secret).unwrap()).unwrap();
        db.import_json::<String>("oauth", &legacy).unwrap();
        assert!(db.get::<String>("oauth").unwrap().is_none());
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_or_wrong_key_never_resets_existing_database() {
        let dir = directory();
        let db = Storage::open(&dir).unwrap();
        db.set("oauth", &"secret").unwrap();
        drop(db);
        let key = fs::read(dir.join("storage.key")).unwrap();
        fs::remove_file(dir.join("storage.key")).unwrap();
        assert!(Storage::open(&dir).is_err());
        assert!(!dir.join("storage.key").exists());
        fs::write(dir.join("storage.key"), [0; 32]).unwrap();
        assert!(Storage::open(&dir).is_err());
        fs::write(dir.join("storage.key"), key).unwrap();
        let db = Storage::open(&dir).unwrap();
        assert_eq!(db.get::<String>("oauth").unwrap().unwrap(), "secret");
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fresh_nonces_and_authenticated_record_names_reject_swaps_and_tampering() {
        let db = Storage::memory();
        let read = || {
            db.0.connection
                .lock()
                .unwrap()
                .query_row("SELECT value FROM records WHERE name='oauth'", [], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .unwrap()
        };
        db.set("oauth", &"same").unwrap();
        let first = read();
        db.set("oauth", &"same").unwrap();
        let mut second = read();
        assert_ne!(first, second);
        assert!(db.decrypt("telegram", &second).is_err());
        second[30] ^= 1;
        assert!(db.decrypt("oauth", &second).is_err());
    }

    #[test]
    fn invalid_legacy_data_is_preserved_and_can_be_retried() {
        let dir = directory();
        let legacy = dir.join("auth.json");
        fs::write(&legacy, b"broken json").unwrap();
        let db = Storage::open(&dir).unwrap();
        assert!(db.import_json::<String>("oauth", &legacy).is_err());
        assert!(legacy.exists());
        assert!(db.get::<String>("oauth").unwrap().is_none());
        fs::write(&legacy, b"\"repaired\"").unwrap();
        db.import_json::<String>("oauth", &legacy).unwrap();
        assert_eq!(db.get::<String>("oauth").unwrap().unwrap(), "repaired");
        drop(db);
        fs::remove_dir_all(dir).unwrap();
    }
}
