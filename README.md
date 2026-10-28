# GPT Alice для Яндекс Станции Mini 3

Rust-приложение для локального управления колонкой и голосового диалога GPT-Live. Исходники, обе Cargo-сборки, lock-файлы, веб-интерфейс и ресурсы находятся в этом репозитории. Сгенерированные бинарники создаются в `dist/` сборочным скриптом; runtime-ключи и настройки в репозиторий не входят.

## Возможности сейчас

- Локальная веб-панель на `http://<IP-колонки>/`, API состояния и управления индикаторами, передними часами, громкостью, mute, Bluetooth и часами/NTP.
- Чтение аппаратных кнопок и mute switch; громкость кнопками меняется шагом 5. В режиме ожидания экран показывает часы, временные состояния возвращаются к часам.
- Вход ChatGPT/Codex через device code flow. Токены хранятся в `auth.json` рядом с бинарником, атомарно записываются с режимом `0600`, не выдаются браузеру и автоматически обновляются.
- Голосовой звонок GPT-Live с физической центральной кнопки или веб-панели. Микрофон и динамик обслуживает отдельный `gpt-live-host`; SFX во время звонка идут через тот же аудиопоток.
- Управление устройством выполняется структурированными командами, классифицированными через Codex OAuth, с локальным исполнением Rust-контроллером. Результат передаётся обратно GPT-Live. Поиск в интернете выполняется приложением через OAuth.
- Временные WebRTC-состояния `disconnected`/`failed` не объявляют звонок потерянным сразу: приложение продолжает мониторинг и восстанавливает статус при возврате `connected`. `closed` завершает звонок.

## Сборка

Для локальной сборки нужны Rust 1.99.1, Docker и `cross` 0.2.5. Cargo-зависимости приложения зафиксированы в двух lock-файлах. Для готовых файлов локальная сборка не нужна: workflow `.github/workflows/release-mini3.yml` собирает бинарники и прикладывает их к опубликованному GitHub Release.

```sh
rustup toolchain install 1.99.1 --profile minimal
rustup target add aarch64-unknown-linux-musl --toolchain 1.99.1
rustup target add armv7-unknown-linux-gnueabihf --toolchain 1.99.1
cargo +1.99.1 install cross --locked --version 0.2.5
./scripts/build-mini3.sh
```

Скрипт запускает сборку основного сервиса как AArch64/musl бинарника и голосового helper как ARMHF/glibc бинарника с ALSA. Результат при успешной сборке помещается в `dist/`:

- `dist/gpt-alice`
- `dist/gpt-live-host`

Helper использует ALSA userspace Mini 3 (`libasound.so.2`, ARMHF); оба бинарника должны быть на колонке рядом. Сборка helper требует ALSA ARMHF headers/library. `Cross.toml` устанавливает их в build image. Workflow проверяет ожидаемые ELF архитектуры и ARMHF loader до публикации артефактов. Полный GitHub Actions запуск пока не подтверждён — его результат появится только после публикации релиза.

Локальный запуск панели (аппаратные функции доступны только на Linux Mini 3):

```sh
cargo run
GPT_ALICE_HTTP_ADDR=127.0.0.1:8080 cargo run
```

Для сборки только основного приложения: `cargo build --release --target aarch64-unknown-linux-musl`. Для helper используйте ARMHF target и доступный ARMHF ALSA sysroot.

## Установка / обновление на колонке

Перед установкой завершите текущий звонок. Укажите адрес и приватный ключ в переменных окружения, чтобы не записывать персональные данные в команды/документацию:

```sh
export MINI3_HOST='<IP-колонки>'
export MINI3_SSH_PORT='2222'
export MINI3_SSH_IDENTITY='/path/to/ssh/id_ed25519'
ssh -p "$MINI3_SSH_PORT" -i "$MINI3_SSH_IDENTITY" "root@$MINI3_HOST" 'mkdir -p /data/opt/gpt-alice'
```

Скопируйте `dist/gpt-alice` и `dist/gpt-live-host` в `/data/opt/gpt-alice/` через `scp`, затем выставьте им `0755` и перезапустите службу:

```sh
scp -P "$MINI3_SSH_PORT" -i "$MINI3_SSH_IDENTITY" dist/gpt-alice dist/gpt-live-host "root@$MINI3_HOST:/data/opt/gpt-alice/"
ssh -p "$MINI3_SSH_PORT" -i "$MINI3_SSH_IDENTITY" "root@$MINI3_HOST" 'chmod 0755 /data/opt/gpt-alice/gpt-alice /data/opt/gpt-alice/gpt-live-host && /etc/init.d/S99zzgpt-alice restart'
```

Статус/логи: `/etc/init.d/S99zzgpt-alice status` и `/data/opt/gpt-alice/server.log`. Текущие файлы перед заменой рекомендуется скопировать в on-device backup.

## Секреты и личные данные

- Не добавляйте `auth.json`, `config.json`, `.env`, SSH-ключи, логи с транскриптами, файлы из `dist/` и содержимое `target/` в Git.
- OAuth-файл `auth.json` создаётся во время авторизации. `config.json` содержит настройки устройства, но не должен содержать секреты.
- Адрес и ключ SSH задавайте локально; в примерах используйте placeholder.
- Перед push проверяйте staged-файлы и полный diff: `git status --short`, `git diff --cached --stat`.

## Обслуживание устройства

Скрипты в `scripts/` включают init-скрипты сервиса и Dropbear, обратимое отключение OTA, а также инструменты карантина данных/удаления штатных Yandex startup tasks. Изменяющие сценарии имеют отдельные `--apply`/`disable` режимы и могут затронуть доступность устройства. Сначала читайте заголовок и `--help`, делайте backup и не выполняйте их для обычного обновления GPT Alice. Для скриптов, работающих по сети, адрес Mini 3 обязателен и передаётся явно.

## Документы

- [PROJECT_OVERVIEW.md](PROJECT_OVERVIEW.md) — текущая архитектура, устройство, файловые пути, сборка, установка и проверенные ограничения.
- [native/gpt-live-host/LICENSE.opencode-gpt-live](native/gpt-live-host/LICENSE.opencode-gpt-live) — лицензия использованной основы аудиопомощника.
- [assets/audio/LICENSE-uisfx-audio.txt](assets/audio/LICENSE-uisfx-audio.txt) — лицензия встроенного звука.
