use std::{
    env, fs,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::runtime::RuntimeController;

const MAX_TIMER_SECONDS: u64 = 99 * 60 + 59;

#[derive(Clone, Serialize, Deserialize)]
pub struct TimerEntry {
    pub id: String,
    pub label: Option<String>,
    pub deadline_unix: u64,
    pub speak_text: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct AlarmEntry {
    pub id: String,
    pub label: Option<String>,
    pub hour: u8,
    pub minute: u8,
    pub deadline_unix: u64,
    pub speak_text: Option<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedSchedules {
    timers: Vec<TimerEntry>,
    alarms: Vec<AlarmEntry>,
}

#[derive(Clone)]
pub struct Scheduler {
    data: Arc<Mutex<PersistedSchedules>>,
    path: PathBuf,
}

impl Scheduler {
    pub fn new() -> Self {
        let mut path = env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        path.push("schedules.json");
        let data = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Self {
            data: Arc::new(Mutex::new(data)),
            path,
        }
    }

    pub fn start(&self, runtime: Arc<RuntimeController>, voice: crate::voice::VoiceController) {
        let scheduler = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let now = unix_now();
                let (remaining, due) = {
                    let Ok(mut data) = scheduler.data.lock() else {
                        continue;
                    };
                    let mut due = Vec::new();
                    let mut changed = false;
                    data.timers.retain(|timer| {
                        if timer.deadline_unix <= now {
                            changed = true;
                            due.push(("таймер", timer.label.clone(), timer.speak_text.clone()));
                            false
                        } else {
                            true
                        }
                    });
                    data.alarms.retain(|alarm| {
                        if alarm.deadline_unix <= now {
                            changed = true;
                            due.push(("будильник", alarm.label.clone(), alarm.speak_text.clone()));
                            false
                        } else {
                            true
                        }
                    });
                    let remaining = data
                        .timers
                        .iter()
                        .map(|timer| timer.deadline_unix - now)
                        .min();
                    if changed {
                        let _ = scheduler.persist_locked(&data);
                    }
                    (remaining, due)
                };
                runtime.set_timer_display(remaining);
                for (kind, label, text) in due {
                    runtime.indicators().set_alarm_active(true);
                    let title = label
                        .map(|label| format!("{kind} «{label}»"))
                        .unwrap_or_else(|| kind.to_owned());
                    let message = text.unwrap_or_else(|| format!("Время. Сработал {title}."));
                    if let Err(error) = voice.announce_schedule(&message).await {
                        voice
                            .log(format!("сработал {title}; озвучивание недоступно: {error}"))
                            .await;
                    } else {
                        voice
                            .log(format!(
                                "сработал {title}; сообщение передано в голосовой звонок"
                            ))
                            .await;
                    }
                    runtime.indicators().set_alarm_active(false);
                }
            }
        });
    }

    pub fn create_timer(
        &self,
        seconds: u64,
        label: Option<String>,
        speak_text: Option<String>,
    ) -> Result<String, String> {
        if !(1..=MAX_TIMER_SECONDS).contains(&seconds) {
            return Err(
                "Длительность таймера должна быть от 1 секунды до 99 минут 59 секунд".into(),
            );
        }
        let timer = TimerEntry {
            id: Uuid::new_v4().to_string(),
            label: clean(label),
            deadline_unix: unix_now() + seconds,
            speak_text: clean(speak_text),
        };
        let message = format!(
            "Таймер {} запущен на {} (id {})",
            timer.label.as_deref().unwrap_or("без названия"),
            format_duration(seconds),
            short_id(&timer.id)
        );
        let mut data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список таймеров".to_owned())?;
        data.timers.push(timer);
        self.persist_locked(&data)?;
        Ok(message)
    }

    pub fn extend_timer(
        &self,
        amount: u64,
        unit: &str,
        id: Option<&str>,
    ) -> Result<String, String> {
        let multiplier = match unit {
            "seconds" => 1,
            "minutes" => 60,
            "hours" => 3600,
            _ => return Err("Единица продления должна быть секундами, минутами или часами".into()),
        };
        if amount == 0 || amount > MAX_TIMER_SECONDS {
            return Err("Продление должно быть положительным значением".into());
        }
        if id.is_some_and(|value| value.trim().len() < 4) {
            return Err("Для выбора таймера нужен id длиной не меньше 4 символов".into());
        }
        let additional_seconds = amount.saturating_mul(multiplier);
        let now = unix_now();
        let mut data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список таймеров".to_owned())?;
        let selected_index = if let Some(id) = id {
            let matches: Vec<usize> = data
                .timers
                .iter()
                .enumerate()
                .filter(|(_, timer)| timer.id == id || timer.id.starts_with(id))
                .map(|(index, _)| index)
                .collect();
            match matches.as_slice() {
                [index] => *index,
                [] => return Err("Таймер с таким id не найден; запроси список таймеров".into()),
                _ => {
                    return Err(
                        "Этот id подходит нескольким таймерам; укажи больше символов".into(),
                    );
                }
            }
        } else {
            data.timers
                .iter()
                .enumerate()
                .min_by_key(|(_, timer)| timer.deadline_unix)
                .map(|(index, _)| index)
                .ok_or_else(|| "Активных таймеров нет".to_owned())?
        };
        let timer = &mut data.timers[selected_index];
        let remaining = timer.deadline_unix.saturating_sub(now);
        let extended_remaining = remaining.saturating_add(additional_seconds);
        if extended_remaining > MAX_TIMER_SECONDS {
            return Err(
                "После продления остаток превысит предел экрана 99:59; продли на меньшее время"
                    .into(),
            );
        }
        timer.deadline_unix = timer.deadline_unix.saturating_add(additional_seconds);
        let message = format!(
            "Таймер {} продлён на {}; осталось {}, id {}",
            timer.label.as_deref().unwrap_or("без названия"),
            format_duration(additional_seconds),
            format_duration(extended_remaining),
            short_id(&timer.id)
        );
        self.persist_locked(&data)?;
        Ok(message)
    }

    pub fn create_alarm(
        &self,
        hour: u8,
        minute: u8,
        offset: i16,
        label: Option<String>,
        speak_text: Option<String>,
    ) -> Result<String, String> {
        if hour > 23 || minute > 59 {
            return Err("Время будильника должно быть в формате 00:00–23:59".into());
        }
        let now = unix_now();
        let local_now = (now as i64 + i64::from(offset) * 60).rem_euclid(86_400) as u64;
        let target = u64::from(hour) * 3600 + u64::from(minute) * 60;
        let mut delta = (target + 86_400 - local_now) % 86_400;
        if delta == 0 {
            delta = 86_400;
        }
        let alarm = AlarmEntry {
            id: Uuid::new_v4().to_string(),
            label: clean(label),
            hour,
            minute,
            deadline_unix: now + delta,
            speak_text: clean(speak_text),
        };
        let message = format!(
            "Будильник установлен на {:02}:{:02} (id {})",
            hour,
            minute,
            short_id(&alarm.id)
        );
        let mut data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список будильников".to_owned())?;
        data.alarms.push(alarm);
        self.persist_locked(&data)?;
        Ok(message)
    }

    pub fn list_timers(&self) -> Result<String, String> {
        let data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список таймеров".to_owned())?;
        if data.timers.is_empty() {
            return Ok("Активных таймеров нет".into());
        }
        let now = unix_now();
        Ok(data
            .timers
            .iter()
            .map(|t| {
                format!(
                    "{}: осталось {}, id {}",
                    t.label.as_deref().unwrap_or("Таймер"),
                    format_duration(t.deadline_unix.saturating_sub(now)),
                    short_id(&t.id)
                )
            })
            .collect::<Vec<_>>()
            .join("; "))
    }

    pub fn list_alarms(&self) -> Result<String, String> {
        let data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список будильников".to_owned())?;
        if data.alarms.is_empty() {
            return Ok("Активных будильников нет".into());
        }
        Ok(data
            .alarms
            .iter()
            .map(|a| {
                format!(
                    "{}: {:02}:{:02}, id {}",
                    a.label.as_deref().unwrap_or("Будильник"),
                    a.hour,
                    a.minute,
                    short_id(&a.id)
                )
            })
            .collect::<Vec<_>>()
            .join("; "))
    }

    pub fn timer_stats(&self) -> Result<String, String> {
        let data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список таймеров".to_owned())?;
        let now = unix_now();
        let mut left: Vec<u64> = data
            .timers
            .iter()
            .map(|t| t.deadline_unix.saturating_sub(now))
            .collect();
        left.sort_unstable();
        let range = if let (Some(min), Some(max)) = (left.first(), left.last()) {
            format!(
                "; ближайший {}, самый долгий {}",
                format_duration(*min),
                format_duration(*max)
            )
        } else {
            String::new()
        };
        Ok(format!(
            "Активных таймеров: {}{range}. Допустимая длительность: от 1 секунды до 99 минут 59 секунд",
            left.len()
        ))
    }

    pub fn alarm_stats(&self) -> Result<String, String> {
        let data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список будильников".to_owned())?;
        let mut deadlines: Vec<u64> = data
            .alarms
            .iter()
            .map(|alarm| alarm.deadline_unix)
            .collect();
        deadlines.sort_unstable();
        let bounds = if let (Some(first), Some(last)) = (deadlines.first(), deadlines.last()) {
            format!(
                "; ближайший сработает через {}, самый дальний — через {}",
                format_duration(first.saturating_sub(unix_now())),
                format_duration(last.saturating_sub(unix_now()))
            )
        } else {
            String::new()
        };
        Ok(format!(
            "Активных будильников: {}{bounds}",
            data.alarms.len()
        ))
    }

    pub fn delete_timer(&self, id: Option<String>) -> Result<String, String> {
        if let Some(id) = id {
            return self.delete(Some(id), true);
        }
        let (id, label, remaining) = {
            let data = self
                .data
                .lock()
                .map_err(|_| "Не удалось получить список таймеров".to_owned())?;
            let now = unix_now();
            let timer = data
                .timers
                .iter()
                .min_by_key(|timer| timer.deadline_unix)
                .ok_or_else(|| "Активных таймеров нет".to_owned())?;
            (
                timer.id.clone(),
                timer.label.clone().unwrap_or_else(|| "без названия".into()),
                timer.deadline_unix.saturating_sub(now),
            )
        };
        self.delete(Some(id.clone()), true)?;
        Ok(format!(
            "Ближайший таймер «{label}» остановлен; id {short}, оставалось {}",
            format_duration(remaining),
            short = short_id(&id)
        ))
    }
    pub fn delete_alarm(&self, id: Option<String>) -> Result<String, String> {
        self.delete(id, false)
    }

    pub fn clear_timers(&self) -> Result<String, String> {
        self.clear(true)
    }
    pub fn clear_alarms(&self) -> Result<String, String> {
        self.clear(false)
    }

    fn clear(&self, timer: bool) -> Result<String, String> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить расписание".to_owned())?;
        let count = if timer {
            let count = data.timers.len();
            data.timers.clear();
            count
        } else {
            let count = data.alarms.len();
            data.alarms.clear();
            count
        };
        self.persist_locked(&data)?;
        Ok(format!("Удалено записей: {count}"))
    }

    fn delete(&self, id: Option<String>, timer: bool) -> Result<String, String> {
        let mut data = self
            .data
            .lock()
            .map_err(|_| "Не удалось получить список расписания".to_owned())?;
        if id.as_deref().is_some_and(|value| value.trim().len() < 4) {
            return Err("Для удаления нужен id длиной не меньше 4 символов; запроси список".into());
        }
        let count = if timer {
            data.timers.len()
        } else {
            data.alarms.len()
        };
        if id.is_none() && count > 1 {
            return Err(
                "Есть несколько записей. Сначала запроси список и выбери нужную по id".into(),
            );
        }
        if id.is_none() && count == 0 {
            return Err("Активных записей нет".into());
        }
        let effective_id = id.or_else(|| {
            if timer {
                data.timers
                    .iter()
                    .min_by_key(|entry| entry.deadline_unix)
                    .map(|entry| entry.id.clone())
            } else {
                data.alarms
                    .iter()
                    .min_by_key(|entry| entry.deadline_unix)
                    .map(|entry| entry.id.clone())
            }
        });
        let (removed, remaining) = if timer {
            let before = data.timers.len();
            data.timers
                .retain(|entry| !matches_id(&entry.id, effective_id.as_deref()));
            (before - data.timers.len(), data.timers.len())
        } else {
            let before = data.alarms.len();
            data.alarms
                .retain(|entry| !matches_id(&entry.id, effective_id.as_deref()));
            (before - data.alarms.len(), data.alarms.len())
        };
        if removed == 0 {
            return Err(
                "Не нашёл соответствующий элемент. Сначала запроси список, чтобы выбрать id".into(),
            );
        }
        self.persist_locked(&data)?;
        let title = if timer {
            "таймер"
        } else {
            "будильник"
        };
        Ok(format!("{title} удалён; осталось {remaining}"))
    }

    fn persist_locked(&self, data: &PersistedSchedules) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(data)
            .map_err(|e| format!("Не удалось сериализовать расписание: {e}"))?;
        let temp = self.path.with_extension("json.tmp");
        fs::write(&temp, bytes)
            .map_err(|e| format!("Не удалось записать расписание рядом с программой: {e}"))?;
        fs::rename(&temp, &self.path).map_err(|e| format!("Не удалось сохранить расписание: {e}"))
    }
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|s| s.trim().chars().take(240).collect())
        .filter(|s: &String| !s.is_empty())
}
fn matches_id(full: &str, requested: Option<&str>) -> bool {
    requested.is_some_and(|id| full == id || full.starts_with(id))
}
fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn format_duration(seconds: u64) -> String {
    let d = seconds / 86400;
    let h = seconds % 86400 / 3600;
    let m = seconds % 3600 / 60;
    let s = seconds % 60;
    if d > 0 {
        format!("{d} д {h} ч {m} мин")
    } else if h > 0 {
        format!("{h} ч {m} мин")
    } else if m > 0 {
        format!("{m} мин {s} с")
    } else {
        format!("{s} с")
    }
}
