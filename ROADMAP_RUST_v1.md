# Qembed Object Runtime — Rust Development Roadmap

> **Файл:** `ROADMAP.md`  
> **База:** Qembed Object Runtime M0 v2.3.3; ADR RT-039…RT-048  
> **Фокус:** только Rust-разработка и тестирование Rust-кода  

**Toolchain policy (2026-11-21):** проект использует Rust 1.100.0 как локальный toolchain и MSRV. Версия закреплена в `rust-toolchain.toml`, `[workspace.package].rust-version` и CI. Обновление Rust выполняется отдельным изменением с полным PostgreSQL 15/16/19 test matrix.
> **Первый milestone:** M1a — walking skeleton `create → publish → invoke → retrieve result` на mock ChatProvider.

---

## 1. Принципы разработки

**Toolchain policy (2026-11-21):** проект использует Rust 1.100.0 как локальный toolchain и MSRV. Версия закреплена в `rust-toolchain.toml`, `[workspace.package].rust-version` и CI. Обновление Rust выполняется отдельным изменением с полным PostgreSQL 15/16/19 test matrix.

1. Этот roadmap описывает только Rust-реализацию. SQL remediation, исправление схемы, эксплуатационные и юридические работы сюда не входят.
2. Архитектура M0 и шесть crates считаются зафиксированными:
   - `qembed-domain`
   - `qembed-store`
   - `qembed-llm`
   - `qembed-tools`
   - `qembed-runtime`
   - `qembed-api`
3. `qembed-eval` — binary target внутри `qembed-runtime`, не отдельный crate.
4. Каждая задача заканчивается тестами. Нельзя закрыть задачу только потому, что код компилируется.
5. Сначала unit tests, затем integration tests на границе соответствующего слоя. Для persistence, RLS, транзакций, CAS и fencing нужны тесты с реальной PostgreSQL, а не SQLite/mock repository.
6. Внешние сетевые вызовы не выполняются внутри SQL-транзакций.
7. Все tenant-scoped repository operations принимают `&Scope`; tenant и authenticated Principal context задаются в транзакции через transaction-local `app.tenant_id` и `app.principal_id`.
8. Invocation и ToolExecution имеют независимые leases/fencing tokens. Переходы состояния используют CAS по tenant, ID, token и version.
9. `Unknown` не является основанием для автоматического retry: требуется reconciliation либо доказанная upstream idempotency со стабильным ключом.
10. Новые архитектурные решения не вводятся молча: при необходимости отдельный ADR.
11. PostgreSQL — единственный источник истины для Invocation, budgets, events, Outbox и результатов. NATS JetStream — транспорт доставки, а не authoritative store бизнес-состояния.
12. Все асинхронные команды публикуются через Transactional Outbox; публикация в JetStream выполняется только после commit исходной PostgreSQL-транзакции.
13. Доставка считается at-least-once. Relay, consumers и recovery обязаны быть идемпотентными; exactly-once для бизнес-операций не обещается.
14. PubAck означает только принятие сообщения JetStream. Outbox сохраняется по retention policy, а потерянная/застрявшая доставка восстанавливается из PostgreSQL.
15. NATS transport скрыт за domain ports. Бизнес-правила и Store не зависят от async-nats или конкретного broker API.
16. Микросервисные границы вводятся через независимо запускаемые process modes и versioned message contracts; не добавлять crate или отдельный сервис без отдельного обоснования.

### Размер задач

- **XS:** до нескольких часов.
- **S:** 0.5–1.5 дня.
- **M:** 2–4 дня.
- **L:** 5+ дней; перед реализацией желательно дополнительно декомпозировать.

Оценки ориентировочные. Реальный размер зависит от выбранных библиотек и состояния репозитория.

---

## 2. Crates и границы ответственности

| Crate | Ответственность | Ограничения |
|---|---|---|
| `qembed-domain` | IDs, entities, `Scope`, `Principal`, state machines, policy contracts (включая broker-neutral publish/consume contracts), ports, `AllowedCommand` | Без SQLx, HTTP framework, provider SDK, async-nats и сетевого I/O |
| `qembed-store` | SQLx repositories, transaction context, persistence, CAS/fencing | Без HTTP handlers и provider-specific logic |
| `qembed-llm` | ChatProvider adapters, capability negotiation, structured output, usage/error mapping | Без orchestration и tool authorization |
| `qembed-tools` | Tool registry adapter, executor/reconciler, SSRF-safe networking, encrypted payload handling | Не принимает policy verdict самостоятельно |
| `qembed-runtime` | Orchestration, budgets, invocation/worker lifecycle, retrieval coordination | Без HTTP transport и SQL query details |
| `qembed-api` | Axum handlers, auth boundary, DTO/OpenAPI, composition root и concrete infrastructure wiring для API/relay/worker process modes | Handler-ы не содержат бизнес-правила; broker SDK не проникает в domain/runtime contracts |

**Dependency rule:** domain не зависит от adapters; adapters реализуют domain ports; runtime координирует domain и ports; Axum handlers вызывают runtime. MessagePublisher/MessageConsumer остаются broker-neutral ports; async-nats и JetStream-specific types разрешены только в concrete infrastructure adapter/composition root внутри qembed-api. qembed-store отвечает за PostgreSQL Outbox и durable business state, но не вызывает NATS. Один и тот же Rust artifact может запускаться в режимах API, outbox-relay и worker; режимы должны быть независимо масштабируемыми процессами. Handler modules ограничиваются qembed-domain и qembed-runtime. Не добавлять седьмой crate. Tower используется как HTTP service/middleware abstraction; он не заменяет Axum и не переносит бизнес-логику в middleware.

---

## 3. Общий тестовый стандарт

Для каждой задачи ниже тестирование входит в Definition of Done. Применять подходящий уровень:

- **Unit:** чистая логика, state transitions, canonicalization, validation, mapping.
- **Compile-fail:** гарантии типов и невозможность запрещённых операций на этапе компиляции.
- **Integration:** взаимодействие crates и реальных адаптеров.
- **Database integration:** PostgreSQL, реальные роли, RLS, транзакции, конкурентные сессии.
- **End-to-end:** HTTP/API → runtime → store/provider → результат.

Минимальные команды CI:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo sqlx prepare --check
SQLX_OFFLINE=true cargo check --workspace
```

Для SQLx query/migration changes обновлять metadata. Тесты, зависящие от PostgreSQL, должны запускаться в отдельном CI job с зафиксированной версией PostgreSQL и pgvector.

---

## 4. Этап E0 — Rust workspace и developer tooling

**Результат:** пустой, но воспроизводимый workspace, в котором удобно и безопасно развивать проект.

- [ ] **RUST-001 (S)** Создать Cargo workspace и шесть crates.
  - Реализация: manifests, `src/lib.rs`/`main.rs`, workspace members, минимальные targets.
  - Тесты: `cargo check --workspace`; smoke test запуска `qembed-eval`.
  - DoD: все crates собираются; `qembed-eval` — bin-target `qembed-runtime`.

- [ ] **RUST-002 (S)** Централизовать версии зависимостей и workspace metadata.
  - Реализация: `[workspace.dependencies]`, edition, `rust-version`, общие lint settings.
  - Тесты: `cargo tree -d`; CI-проверка минимально поддерживаемой версии Rust.
  - DoD: версии и политики зависимостей заданы централизованно.

- [ ] **RUST-003 (S)** Зафиксировать crate dependency boundaries.
  - Реализация: composition root в `qembed-api`; handler modules не импортируют concrete adapters.
  - Тесты: dependency graph check (например, cargo-deny/скрипт архитектурных правил); сборка всех targets.
  - DoD: нет циклических зависимостей и запрещённых импортов.

- [ ] **RUST-004 (S)** Настроить rustfmt, Clippy и базовые lint rules.
  - Тесты: `cargo fmt --check`; Clippy на workspace/all-targets с `-D warnings`.
  - DoD: проверки обязательны в CI.
- [ ] **RUST-005 (S)** Настроить CI pipeline для Rust.
  - Реализация: format → check → clippy → unit tests; кэш Cargo.
  - Тесты: проверить успешный pipeline и намеренно сломанный lint/test.
  - DoD: PR не проходит при ошибке любой обязательной проверки.

- [ ] **RUST-006 (S)** Подготовить PostgreSQL integration-test harness.
  - Реализация: выбрать один основной способ — Testcontainers или docker compose; единые test fixtures.
  - Тесты: тест поднимает БД, подключается, проверяет версию и корректно завершает окружение.
  - DoD: локально и в CI доступен воспроизводимый PostgreSQL test environment.

- [ ] **RUST-007 (S)** Подключить SQLx и migration runner.
  - Реализация: единая точка создания pool; явная команда миграций; конфигурация pool.
  - Тесты: integration test запускает миграции на пустой тестовой БД и проверяет доступность соединения.
  - DoD: приложение не выполняет скрытые DDL-изменения при обычном старте.

- [ ] **RUST-008 (S)** Настроить SQLx offline metadata.
  - Реализация: `cargo sqlx prepare`, хранение `.sqlx`.
  - Тесты: `cargo sqlx prepare --check` и `SQLX_OFFLINE=true cargo check --workspace`.
  - DoD: CI проверяет актуальность metadata без runtime DB connection.

- [ ] **RUST-009 (XS)** Добавить developer README и тестовые команды.
  - Тесты: пройти инструкции на чистом checkout/окружении.
  - DoD: разработчик может собрать workspace и запустить unit/integration tests по README.

**Gate E0:** workspace собирается; CI зелёный; PostgreSQL harness и SQLx offline workflow работают.

---

## 5. Этап M1a — Domain contracts

**Результат:** типизированные контракты, на которых строятся остальные crates.

- [ ] **RUST-010 (S)** Реализовать strongly typed IDs.
  - Объекты: Tenant, Principal, Object, ObjectVersion, Caller, Invocation, Snapshot, Event и остальные используемые ID.
  - Тесты: parse/format/serde round-trip; некорректные значения; compile-time проверки несовместимости разных ID.
  - DoD: нельзя случайно передать ID одного типа вместо другого.

- [x] **RUST-011 (S)** Реализовать `Scope` и `Principal`.
  - Тесты: unit tests на создание scope из доверенного auth context; отрицательные тесты на пустой/некорректный scope.
  - DoD: HTTP DTO не может самостоятельно создать доверенный `Scope`.

- [x] **RUST-012 (M)** Реализовать domain entities и state enums.
  - Объекты: Object, ObjectVersion, Invocation, Snapshot.
  - Тесты: таблицы допустимых/недопустимых state transitions; property tests для инвариантов.
  - DoD: переходы выражены методами/типами, а не произвольными строками.

- [x] **RUST-013 (S)** Определить domain ports.
  - Контракты: authentication, execution store, provider, clock/ID generation там, где это нужно.
  - Тесты: compile tests на реализации test doubles; domain crate собирается без adapter dependencies.
  - DoD: domain не зависит от SQLx, HTTP framework или provider SDK.

- [x] **RUST-014 (M)** Реализовать `AllowedCommand` capability.
  - Реализация: приватные поля; mint boundary только в `qembed-domain::policy`.
  - Тесты: compile-fail tests на `Clone`, Serialize/Deserialize, Default и внешнее создание; unit tests на валидную mint boundary.
  - DoD: тип `!Clone`, `!Serialize`, `!Deserialize`, `!Default`; произвольный caller не может его сконструировать.

- [x] **RUST-015 (S)** Добавить compile-fail suite для security-sensitive domain contracts.
  - Реализация: единый набор rustdoc `compile_fail` контрактов в документации `qembed-domain`; без дополнительной test-зависимости.
  - Тесты: внешнее создание `Principal`, `Scope`, `AllowedCommand`; вызов trusted constructors; Deserialize для `Principal`/`Scope`; запрещённые traits `AllowedCommand`.
  - DoD: отдельный CI step `cargo test -p qembed-domain --doc` проверяет, что все запретные примеры действительно не компилируются.

---

## 6. Этап M1a — Store и tenant-scoped persistence

**Результат:** реальные repository operations с tenant isolation и атомарными записями.

- [x] **RUST-016 (M)** Реализовать SQLx transaction wrapper, принимающий `&Scope`.
  - Реализация: `begin_tenant_transaction` создаёт SQLx transaction и устанавливает `app.tenant_id` через parameterized `set_config(..., true)`; wrapper предоставляет connection и consuming commit/rollback.
  - Тесты: PostgreSQL integration проверяет tenant setting внутри транзакции и его отсутствие после commit и rollback на одном pooled connection; compile-fail doctest доказывает, что начать транзакцию без `&Scope` нельзя.
  - DoD: tenant context устанавливается через transaction-local setting в каждой tenant-транзакции.

- [x] **RUST-017 (M)** Реализовать repository conventions и typed query helpers.
  - Реализация: `fetch_all`, `fetch_optional` и `execute` принимают только `TenantTransaction`; repository convention — принимать trusted `&Scope`, открывать tenant transaction и выполнять запросы через typed SQLx helpers.
  - Тесты: PostgreSQL integration проверяет RLS isolation для tenant A/B и fail-closed поведение без tenant context; compile-fail doctest запрещает передавать обычный pool вместо tenant transaction.
  - DoD: tenant-owned query helpers не принимают непроверенный tenant ID или обычный pool; tenant context задаётся только из trusted `Scope`.

- [x] **RUST-018 (M)** Реализовать principal/caller persistence и минимальный Owner authentication path.
  - Реализация: PostgreSQL хранит tenant-bound Owner principals и SHA-256 digests высокоэнтропийных API-токенов; SQLx AuthenticationPort adapter проверяет tenant, token, disabled/revoked state; domain `authenticate_owner` создаёт Principal и Scope только после проверки adapter и отклоняет не-Owner identity.
  - Тесты: PostgreSQL integration — валидная аутентификация, неверные credentials, disabled principal, tenant mismatch и persistence только digest; unit — redacted credentials Debug, Owner scope и отказ non-Owner.
  - DoD: runtime получает Principal только после успешной проверки auth boundary; неизвестные, отключённые, отозванные и tenant-mismatched credentials дают одинаковый отказ.

- [x] **RUST-019 (M)** Реализовать Object repository: create object и create draft version.
  - Реализация: `ObjectRepository` принимает только trusted `Scope`, сохраняет Draft Object и Draft ObjectVersion через `TenantTransaction`; PostgreSQL schema использует tenant/object composite FK, глобальную уникальность IDs, уникальный version number в пределах объекта и FK опубликованного version pointer.
  - Тесты: DB integration — создание объекта и draft version, duplicate object/version IDs and version number, tenant mismatch, cross-tenant object reference через composite FK, failed insert rollback; unit/domain boundary — repository отвергает не-Draft сущности.
  - DoD: создание объекта и draft version выполняется в tenant scope; PostgreSQL не допускает dangling или cross-tenant version references.

- [x] **RUST-020 (M)** Реализовать publish operation.
  - Реализация: `ObjectRepository::begin_validation` переводит Draft object в Validating; `publish_version` блокирует строку Object через `SELECT ... FOR UPDATE`, проверяет expected current pointer, переводит новую версию Draft → Published, предыдущую Published → Superseded и атомарно обновляет pointer/state.
  - Тесты: DB integration — первая публикация, замена версии, stale pointer rejection, injected failure на финальном pointer update с проверкой rollback всех изменений, две конкурентные первые публикации с ровно одним победителем.
  - DoD: публикация и обновление object pointer/state атомарны; repository принимает к публикации только Draft version, а ранее опубликованная версия переводится в Superseded без редактирования её содержимого.

- [x] **RUST-021 (M)** Реализовать Snapshot builder и repository.
  - Реализация: `SnapshotBuilder` сохраняет переданные canonical bytes без parse/normalize/re-serialize; `SnapshotRepository` tenant-scoped insert/read, SHA-256 проверяется повторно при reconstitution через `Snapshot::from_persisted`.
  - DB: `snapshots.canonical_json BYTEA` — source of truth; `content_hash BYTEA` хранит SHA-256 точных bytes; `json_projection JSONB` строится из bytes и сверяется trigger guard; UPDATE/DELETE snapshots запрещены.
  - Тесты: deterministic builder; PostgreSQL immutable insert/read и exact-byte round-trip; hash mismatch обнаруживается при read; projection mismatch отклоняется DB guard; UPDATE/DELETE отклоняются.
  - DoD: canonical bytes не подменяются JSONB projection; repository возвращает Snapshot только после проверки hash.

- [x] **RUST-022 (M)** Реализовать JCS canonicalization/validation boundary.
  - Реализация: domain module jcs предоставляет canonicalize_json и validate_canonical_json на базе serde_jcs (RFC 8785); строгий Serde visitor отклоняет duplicate object keys до сериализации; Snapshot::new и from_persisted требуют уже канонические bytes.
  - Тесты: RFC 8785 number vector; UTF-16 key ordering; duplicate keys на вложенных уровнях; NaN/Infinity; invalid UTF-8 и malformed JSON; noncanonical whitespace/key order/number/string escapes; idempotence corpus; Snapshot construction rejects noncanonical bytes.
  - DoD: Rust отвергает некорректный или неканонический snapshot до persistence; canonicalization выдаёт RFC 8785 bytes, validation не переписывает вход.

- [x] **RUST-023 (S)** Реализовать DB hash/projection integration tests и store mapping.
  - Реализация: явно проверить mapping `canonical_json BYTEA` и `content_hash` в domain Snapshot; `json_projection JSONB` остаётся отдельной производной колонкой.
  - Тесты: PostgreSQL round-trip сравнивает исходные байты, SHA-256 и отличающуюся текстовую JSONB-проекцию; projection mismatch отвергается DB guard; hash mismatch отвергается Domain при чтении; корректные hash/projection не позволяют принять неканоничные source bytes.
  - DoD: store не подменяет canonical bytes JSONB projection.

- [x] **RUST-024 (M)** Реализовать append-only event write и transactional outbox insert.
  - Реализация: ObjectRepository записывает события начала validation и публикации версии вместе с outbox message в той же tenant-транзакции, что и state transition; delivery identity привязана к EventId; события защищены DB trigger от UPDATE/DELETE.
  - Тесты: PostgreSQL integration — state+event+outbox commit; forced failure откатывает state/event/outbox; duplicate delivery identity отклоняется unique constraint.
  - DoD: critical event и outbox создаются атомарно с state transition; повторная delivery identity не создаёт вторую outbox запись.

- [x] **RUST-025 (S)** Реализовать typed event DTO и payload allowlist.
  - Реализация: Store принимает только приватные Serialize-only DTO для известных Object lifecycle events; event type/topic выводится из enum variant; payload сериализуется из allowlisted полей, произвольный JSON и deserialization boundary отсутствуют.
  - Тесты: serialization tests проверяют точный набор полей для validation-started/version-published; negative assertions исключают secrets, credentials, arguments и raw payload.
  - DoD: произвольный JSON и чувствительные payloads не проходят в event API.

- [x] **RUST-026 (S)** Реализовать repository CAS helper.
  - Реализация: `execute_cas` выполняет условный SQL update только через `TenantTransaction`; ровно одна изменённая строка означает успех, ноль — stale-state conflict, больше одной — invariant violation. Object validation/publication используют общий helper.
  - Тесты: PostgreSQL integration с двумя независимыми backend sessions подтверждает stale predicate conflict после commit победителя; отдельный тест проверяет отказ CAS при обновлении нескольких строк и rollback.
  - DoD: успешный CAS требует ровно одной изменённой строки.

- [x] **RUST-027 (M)** Покрыть RLS и composite FK интеграционными тестами.
  - Тесты: non-owner RLS role проверяет tenant-scoped SELECT/INSERT/UPDATE/DELETE, cross-tenant writes отклоняются, отсутствие tenant context возвращает пустой read scope; прямые PostgreSQL inserts подтверждают cross-tenant rejection для object/version, published-version pointer, snapshot provenance и event/outbox FK.
  - DoD: проверяется поведение PostgreSQL constraints и RLS под отдельной non-owner ролью, а не только mock repository.

- [x] **RUST-028 (S)** Проверить append-only и restricted data access через store API.
  - Тесты: PostgreSQL integration под отдельной non-owner runtime role подтверждает SELECT и отказ UPDATE/DELETE для snapshots и events; отказ привилегий проверяется по SQLSTATE 42501. Существующие owner-level tests отдельно подтверждают, что immutable triggers отклоняют прямые UPDATE/DELETE по SQLSTATE 55000.
  - Store API: SnapshotRepository предоставляет только insert/get; compile-fail doctests фиксируют отсутствие update/delete. Event append helper остаётся crate-private, generic event writer не экспортируется; compile-fail doctest фиксирует границу видимости.
  - Scope note: Decision entity/table/repository отсутствуют в текущих Rust domain contracts и migrations. Не вводим неподтверждённую модель ради теста; decision-specific access rules добавляются вместе с будущим контрактом persistence.
  - DoD: существующие immutable snapshots/events доступны только через разрешённые операции; для decisions проверка отложена до появления их Rust persistence contract.

- [ ] **RUST-029 (S)** Добавить тесты role separation для store integration.  - Реализация: отдельные NOLOGIN group roles runtime/dispatcher/admin; реальные LOGIN test identities; RLS на всех tenant-owned таблицах; транзакционный tenant context для pre-auth owner lookup.
  - Тесты: подключения под каждой ролью; positive/negative permission matrix; cross-tenant dispatcher только для outbox; отсутствие tenant context fail-closed.
  - Архитектурный контракт: `docs/STORE_ROLE_MATRIX.md`; migration identity отделена от application roles и требует CREATEROLE.
  - DoD: integration suite подключается под каждой тестируемой ролью, а не только DB owner.

---
## 7. Этап M1a — LLM adapter и Mock ChatProvider

**Результат:** runtime работает с provider через стабильный контракт; тесты не зависят от внешнего API.

- [x] **RUST-030 (S)** Определить `ChatProvider` port и request/response types.
  - Тесты: contract tests для обязательных полей, limits и serialization.
  - DoD: provider SDK-типы не протекают в domain/runtime contracts.

- [x] **RUST-031 (S)** Реализовать deterministic Mock ChatProvider.
  - Реализация: `qembed-llm::MockChatProvider` выбирает script по полному равенству `ChatRequest`, поддерживает response и типизированные Timeout/Unavailable; незаданный запрос и дублирующий script отклоняются.
  - Тесты: exact input/output; TokenUsage fixture; scripted timeout/provider error; missing/duplicate script; 32 concurrent identical calls.
  - DoD: provider immutable и не использует FIFO/shared cursor, mutex, network или clock; результаты конкурентных вызовов не зависят от порядка планирования. Архитектура описана в `docs/DETERMINISTIC_MOCK_CHAT_PROVIDER.md`.

- [x] **RUST-032 (S)** Реализовать structured output modes `JsonOnly` и `None`.
  - Реализация: `StructuredOutputMode` входит в `ChatRequest`; старые serialized requests и `ChatRequest::new` по умолчанию используют `None`. `qembed-llm` проверяет поддержку режима до provider I/O и валидирует нормализованный JSON response.
  - Тесты: mode serialization/default compatibility; valid JSON; malformed/trailing/prose response; plain text in `None`; unsupported `JsonOnly` rejected before request.
  - DoD: structured output contract provider-neutral; результат валиден для запрошенного режима либо возвращается типизированная `UnsupportedMode`/`MalformedJson` ошибка. Capability discovery остаётся RUST-033.

- [x] **RUST-033 (S)** Реализовать capability negotiation.
  - Реализация: domain `ChatCapabilities` описывает явно настроенные structured-output modes; `ChatProvider::capabilities(model_key)` возвращает локальный профиль без сетевого I/O; `negotiate_request` отклоняет отсутствующий route и неподдерживаемый mode до `complete`.
  - Тесты: mock provider с разными capability profiles; неизвестный route; unsupported `JsonOnly`; `None` всегда поддержан; отсутствие вывода возможностей из model key.
  - DoD: runtime/adapter использует явный capability descriptor, а не угадывает возможности по имени модели.

- [x] **RUST-034 (S)** Реализовать provider error и usage mapping.
  - Реализация: domain `ChatProviderError` задаёт нейтральные категории `Timeout`, `RateLimited` (с необязательной подсказкой задержки), `Unavailable`, `MalformedOutput` и `Other`; `qembed-llm` предоставляет классификацию ошибок адаптера и нормализацию usage.
  - Тесты: mapping для всех категорий, сохранение rate-limit hint, отсутствие автоматической retry-семантики; usage отсутствует/частичный/полный/выходящий за диапазон.
  - DoD: raw SDK errors не покидают adapter; неизвестные и неполные usage не превращаются в фиктивные нули.

---

## 8. Этап M1a — Runtime invocation и API walking skeleton

**Результат:** сквозной flow через реальный API, store и mock provider.

- [x] **RUST-035 (M)** Реализовать idempotency reservation для begin-invocation.
  - Реализация: `qembed-store::idempotency::reserve_invocation_idempotency` резервирует ключ по `(tenant, object, trusted caller, key)` в tenant transaction; одинаковый payload возвращает исходный invocation ID, несовпадающий payload даёт конфликт. Deferred FK позволяет сначала claim, затем вставить invocation в той же транзакции.
  - Тесты: PostgreSQL integration — повтор ключа, конкурентные запросы, payload mismatch, rollback claim и независимость tenant/object/caller scopes.
  - DoD: reservation и invocation фиксируются до provider I/O; TTL и алгоритм хеширования не выдумываются этим слоем.

- [x] **RUST-036 (M)** Реализовать атомарный begin-invocation transaction и SQL-011 Principal/Caller delegation integration.
  - Реализация: отдельные PrincipalId и CallerId; transaction-local app.tenant_id + app.principal_id; SQL-011 resolve_delegated_caller с блокировками; idempotency scoped по ResolvedCaller; explicit InvocationPool; атомарные snapshot/revision pin, invocation insert, critical event и outbox.
  - Тесты: PostgreSQL integration — commit/replay, caller scoping, invalid snapshot rollback, injected failures at invocation/event/outbox writes. SQL clean-install/smoke validation remains a separate qembed-sql gate.
  - Документация: PRINCIPAL_CALLER_DELEGATION.md, BEGIN_INVOCATION_TRANSACTION.md, idempotency contract and Store role matrix.
  - DoD: либо фиксируется весь begin-invocation, либо ничего; provider I/O начинается только после commit.

- [x] **RUST-037 (M)** Реализовать минимальный invocation worker lifecycle.
  - Реализация: durable user input is committed with begin-invocation; worker resumes from persisted Invocation status; provider output is persisted before Verifying → Completed; terminal status/result are committed together where possible. No lease/reclaim semantics here (RUST-038).
  - Тесты: runtime mock-provider tests — claim/process/finish and provider failure; PostgreSQL integration — persisted input, CAS transitions, response durability, restart at Verifying, terminal result retrieval, idempotent replay does not duplicate input.
  - Документация: INVOCATION_WORKER_LIFECYCLE.md, BEGIN_INVOCATION_TRANSACTION.md, README.
  - DoD: invocation lifecycle сохраняется в PostgreSQL и переживает рестарт процесса.

- [x] **RUST-038 (M)** Реализовать invocation lease и fencing-token CAS.
  - Реализация: initial owner claim без смены token; heartbeat только активного владельца; reclaim истёкшей lease с атомарным увеличением token/version; все worker writes проверяют owner, expiry, token и version.
  - Тесты: две независимые PostgreSQL sessions; конкурентный claim; heartbeat; expiry/reclaim; старый worker не может записать assistant output после выдачи нового token.
  - DoD: stale worker не записывает результат и не завершает invocation после reclaim.

- [x] **RUST-038A (S)** Заложить стабильный provider idempotency key в ChatProvider contract.
  - Реализация: runtime выводит key из durable InvocationId и передаёт его в provider request context на каждой попытке; capability явно объявляет поддержку только для adapter-ов, которые действительно передают key upstream.
  - Тесты: одинаковый invocation ID даёт одинаковый key; другой invocation — другой key; default provider path не заявляет idempotency; lease owner/token не участвуют в key.
  - DoD: повтор после lease reclaim использует тот же provider key; exactly-once не обещается для provider-ов без подтверждённой upstream idempotency.

- [x] **RUST-039 (S)** Реализовать read path `GET invocation`.
  - Реализация: tenant-scoped `InvocationReadStore`, runtime query service и минимизированный API response; внутренние tenant/caller/snapshot/trace/lease/fencing/version и незавершённый assistant output не выдаются.
  - Тесты: PostgreSQL integration — queued/running/succeeded/failed, tenant isolation, assistant output скрыт до terminal success; API serialization — только allowlisted response fields.
  - DoD: клиент получает только публичный status/result/stable error code; отсутствующий и чужой invocation дают одинаковый not-found результат.

- [x] **RUST-040 (S)** Реализовать cancellation contract.
  - Реализация: tenant-scoped cancellation port/service/handler; PostgreSQL атомарно переводит nonterminal invocation в Cancelled, фиксирует `cancel_requested_at`, завершает lifecycle, освобождает lease и увеличивает fencing token. Повторная отмена идемпотентна; другой terminal state не перезаписывается.
  - Тесты: PostgreSQL integration — active lease fencing, heartbeat stale-owner rejection, repeated cancel, unknown ID и terminal invocation.
  - DoD: cancellation durable; stale worker больше не может записать результат; API явно сообщает, была ли отмена применена, и не обещает отменить уже совершённый внешний side effect.

- [ ] **RUST-041 (L)** Реализовать Token Budget Reservation & Settlement для mock path; выполнять поэтапно через RUST-041a…041e.
  - Общий инвариант: суммарные активные reservations и учтённый расход не превышают эффективный лимит invocation/object; immutable snapshot cap ограничивает live object budget, live budget может только ужесточить snapshot cap.
  - [x] **RUST-041a (S) — Domain budget calculations.** Provider-neutral domain types/functions: effective limit = `min(snapshot.limits.max_tokens_per_invocation, live object budget)`; mock input estimate = `ceil(input_utf8_bytes / 3) * 1.3`, округление вверх; output allowance ограничен остатком; normalized provider usage заменяет estimate; отсутствующий usage использует non-zero reservation estimate и помечается degraded. Unit/property tests на нули, overflow, границы, UTF-8 и usage fallback. DoD: чистая логика в `qembed-domain`, без SQL/provider SDK и без silent-unlimited/zero fallback.
  - [x] **RUST-041b (M) — Atomic budget reservation.** Реализовано: snapshot/live per-invocation cap; aggregate UTC-day object token limit across both pools; per-caller UTC-day token limit; per-object active concurrency limit; атомарные reservation и счётчики в begin transaction; idempotent replay без двойного reserve. PostgreSQL сериализует резервирования через `object_budgets FOR UPDATE`; `anonymous_share_pct` не имеет runtime-семантики. Integration tests: independent sessions racing for the final daily budget, pool aggregation, caller/object/concurrency rejection, snapshot validation, replay and rollback. DoD: PostgreSQL — источник истины; preflight check не считается защитой.
  - **RUST-041c (M) — Provider usage settlement.** Нормализовать provider usage на adapter boundary; заменять estimate фактическим usage; идемпотентно обновлять `usage_counters` и `caller_usage_counters`; отсутствие usage обрабатывается по degraded fallback contract, никогда как ноль. Tests: normalized usage, repeated settlement, missing/malformed usage, transaction rollback. DoD: повторный settlement не увеличивает расход повторно.
  - **RUST-041d (M) — Failure and cancellation reconciliation.** Определить reservation lifecycle для pre-provider failure, confirmed provider failure, cancellation и ambiguous outcome; не освобождать автоматически reservation после timeout/crash, если provider мог принять запрос; сохранять возможность reconciliation и учитывать stable provider idempotency key только при реально подтверждённой upstream поддержке. Failure-injection tests на crash до отправки, после отправки, после provider response и до settlement; cancellation/stale worker. DoD: Unknown outcome не трактуется как zero usage и не вызывает небезопасный retry/refund.
  - **RUST-041e (M) — Concurrency and integration acceptance.** Сквозные PostgreSQL acceptance tests: параллельные reservations на одном object/caller, budget exhaustion, duplicate delivery/idempotency, settlement race, commit/rollback, worker restart и cancellation. DoD: доказано отсутствие превышения бюджета под конкуренцией; все проверки запускаются в CI на реальной PostgreSQL.
  - Порядок: закрыть и проверить 041a до начала 041b; затем 041b → 041c → 041d → 041e. Каждый подпункт имеет отдельный PR и собственный CI gate; merge только после явного подтверждения.
  - SQL physical schema не расширять без отдельного обоснования и согласования с каноническим `qembed-sql` contract.

- [ ] **RUST-042 (S)** Реализовать typed runtime errors и API error mapping.
  - Тесты: unit на все error mappings; API tests на стабильные error codes и отсутствие SQL/provider internals.
  - DoD: ошибки пригодны для клиента и не раскрывают внутренние детали.

- [ ] **RUST-043 (S)** Реализовать health/readiness endpoints.
  - Тесты: API integration — healthy DB, unavailable DB, migration mismatch; проверка отсутствия secrets в response.
  - DoD: readiness проверяет обязательные зависимости.

- [x] **RUST-044-T (M)** Использовать Axum + Tower как единый HTTP transport stack для `qembed-api` до реализации оставшихся Owner API endpoints.
  - Основание: Owner API ещё небольшой; миграция до добавления оставшихся endpoints сокращает повторную работу и удерживает единый HTTP stack.
  - Архитектурное решение: Axum — HTTP router/handler framework; Tower — service/middleware layer. Сохранить HTTP/JSON Owner API и текущие auth/tenant boundaries. Connect/gRPC/Protobuf в эту задачу не входят и требуют отдельного ADR/решения. Полное решение: [ADR-RT-053](docs/ADR-RT-053-AXUM-TOWER-HTTP-TRANSPORT.md).
  - MSRV: workspace minimum Rust 1.100.0; `rust-toolchain.toml` фиксирует Rust 1.100.0, а CI запускает основной matrix на той же версии. Исторические записи о Rust 1.85 ниже относятся к фактически выполненным M1a PR и CI runs.
  - Реализация: `qembed-api` предоставляет Axum Router, Axum handlers и Tower middleware; health/readiness и Owner routes собраны через общий composition root. `actix-web` не входит в workspace dependencies или crate manifests.
  - Сохранить без изменения публичные HTTP contracts: paths, methods, headers, status codes, DTOs, error codes, response minimization, Bearer auth и недоверенный X-Tenant-ID selector. Не переносить SQLx или бизнес-логику в handlers/middleware.
  - Тесты: Axum router/service integration для каждого endpoint; auth/tenant negative cases; health/readiness behavior; serialization/error mapping; реальные PostgreSQL API integration tests остаются на PostgreSQL и не заменяются router mocks.
  - Граница ответственности: `qembed-api` предоставляет Router, middleware и readiness. Запуск listener, process startup/shutdown и graceful shutdown принадлежат будущему server executable/host и не являются обязанностью API crate; их проверка не блокирует закрытие этой задачи.
  - DoD: все API routes и middleware используют Axum + Tower; HTTP contract regression suite и PostgreSQL API integration tests проходят. На момент миграции Rust 1.85 MSRV и stable CI были зелёными; текущая workspace toolchain/MSRV зафиксирована отдельно в Toolchain policy.
  - Порядок (выполнен): архитектурное решение → dependency compatibility audit → Axum + Tower migration → CI/tests → Owner API endpoints на Axum.
- [x] **RUST-044 (M)** Собрать Owner API: create object/version, publish, invoke, inspect invocation.
  - Архитектурный контракт: [Owner API and trusted publication validation](docs/OWNER_API_PUBLICATION_VALIDATION.md).
  - Первый кодовый срез: bounded domain validator `m0-r3-bounded-budget-v1`; проверяет только подтверждённый положительный integer `limits.max_tokens_per_invocation` и выпускает snapshot-bound opaque evidence. Полная семантическая валидация не заявляется.
  - Одобренное улучшение (2026-11-19): `ObjectValidator::validate_with_report()` возвращает evidence вместе с явным coverage report. Report перечисляет проверенный invariant и непроверенные области, связанные с GAP-044-01…05; статус gap означает «не проверено», а не автоматическую недопустимость произвольного JSON.
  - Coverage report — информационный результат, не authorization capability и не замена opaque evidence. Существующий `validate()` сохраняет совместимый путь получения evidence.
  - Новые semantic rules не добавлять без нормативного источника или отдельного согласования; изменение самих правил требует нового immutable validator version.
  - Owner API не управляет переходами `Validating`/`Published`; публикация требует успешного результата доверенного validator, привязанного к tenant/object/version/snapshot (и hash/ruleset version).
  - Snapshot JCS/integrity и semantic object validation — разные проверки; известный M0 invariant `limits.max_tokens_per_invocation` — положительный JSON integer.
  - Draft version + snapshot создаются одной Store-транзакцией; validation/network I/O выполняются вне неё; publish сохраняет expected-pointer CAS и атомарность Store.
  - Identical-content version semantics утверждены в qembed-sql ADR RUST-044 (см. архитектурный документ).
  - Тесты: API integration на каждый endpoint; auth/tenant negative cases; DTO validation; validator identity binding; PostgreSQL rollback и stale-pointer race cases.
  - Semantic coverage tests: подтверждённый budget rule помечен как validated; каждая открытая specification gap явно помечена not_validated; report не объявляет полное покрытие и не меняет результат publication authorization.
  - Одобренный HTTP authentication contract (2026-11-20): `Authorization: Bearer <owner-token>` + `X-Tenant-ID: <uuid>`. Tenant header — только недоверенный selector для credential lookup; `Scope` создаётся после успешной проверки persisted owner credential. Детали: [RUST-044 Owner API implementation](docs/RUST-044_OWNER_API_IMPLEMENTATION.md).
  - Реализованы Owner endpoints: create object, create version + Snapshot, publish version и start invocation; Inspect Invocation возвращает tenant-scoped минимизированную проекцию, missing и cross-tenant ID дают одинаковый 404.
  - DoD: handler-ы только валидируют transport input и вызывают runtime use cases; Store публикует только с доверенным доказательством validation. PostgreSQL/API integration и security tests проходят; stable и Rust 1.85 MSRV CI зелёные.
- [x] **RUST-045 (M)** Добавить полный walking-skeleton end-to-end test.
  - Реализован `crates/qembed-api/tests/walking_skeleton_e2e.rs`: Owner HTTP create → create version/Snapshot → publish → invoke → retrieve result; реальный PostgreSQL и deterministic `MockChatProvider`.
  - Проверены повторный idempotency key (тот же Invocation ID и ровно один набор durable side effects), teardown первого API/pool и reconnect со свежим Runtime worker, а также cross-tenant 404.
  - Worker после restart адресно запускается по сохранённому Invocation ID; это не заявляет наличие outbox dispatcher или production process supervisor. Caller delegation и Object budget создаются тестовыми provisioning fixtures вне Owner API.
  - DoD: flow проходит через HTTP/API, runtime, PostgreSQL и Mock ChatProvider без внешнего сервиса; stable и Rust 1.85 MSRV CI зелёные.
- [x] **RUST-046 (S)** Проверить DB-backed provenance constraints, относящиеся к M1a.
  - T-04/T-21: прямые PostgreSQL INSERT probes отвергают запрещённые provenance claims; Tool/Observed служит положительным контролем.
  - T-27: PostgreSQL RLS probes проверяют Caller A/B isolation для episodic memories, embeddings и conflict edges, fail-closed без Caller context и очистку transaction-local Caller context после commit.
  - Evidence: [RUST-046 DB provenance report](docs/RUST-046_DB_PROVENANCE_EVIDENCE.md); canonical SQL smoke run #44801382870 на PostgreSQL 16 + pgvector 0.8.7, SQL HEAD `bfa5d885ecd3ee96e59b57e4b2d17d172c532e6b`.
  - Граница: application-level retrieval/provenance verifier не реализован и остаётся M2; SQL GUC trust model не защищает от произвольного SQL под runtime DB role.

- [x] **RUST-047 (S)** Добавить failure-injection tests для M1a crash boundaries.
  - Реализованы PostgreSQL integration tests: termination backend до COMMIT с атомарным rollback всех invocation side effects; commit + teardown/reconnect; provider timeout → durable OutcomeUnknown; worker restart не повторяет неоднозначный provider call.
  - Evidence: [RUST-047 failure-injection report](docs/RUST-047_FAILURE_INJECTION.md); PR #69 merged as `3f49d5847c0273caa7efc63aa73160fac896156f`, stable и Rust 1.85 MSRV jobs passed.
  - Граница: не заявлены production outbox dispatcher/redelivery, process supervisor, provider-side idempotency или external outcome reconciliation.

- [x] **RUST-048 (M)** Прогнать M1a acceptance suite.
  - Тесты: все применимые M1a cases из `acceptance_T-01_to_T-64.md`, включая tenant/Object isolation, idempotency, snapshots/ToolVersion pinning, ordered event streams, secret hygiene, RLS catalog guard, composite claim integrity, API и capability compile-fail.
  - Evidence: [M1a acceptance matrix](docs/RUST-048_M1A_ACCEPTANCE_MATRIX.md); Rust stable + Rust 1.85 MSRV на PostgreSQL 15.10/16.4/19.0 — run [#44809515677](https://github.com/qembed/qembed-object-runtime/actions/runs/44809515677); canonical SQL 359/359 на PostgreSQL 15/16/19 + pgvector 0.8.7 — run [#44808543381](https://github.com/qembed/qembed-sql/actions/runs/44808543381).
  - DoD: результат каждого применимого M1a test ID зафиксирован; все обязательные Rust/DB CI jobs зелёные. M2 verifier/retrieval остаются вне M1a.

- [x] **RUST-049 (S)** Подготовить M1a test evidence.
  - Реализация: [evidence artifact contract](docs/RUST-049_M1A_TEST_EVIDENCE.md); stable CI публикует отдельный artifact для PostgreSQL 15/16/19 с JUnit/nextest report, DB/pgvector versions, test logs и concurrency test results.
  - Evidence: run [#44811246397](https://github.com/qembed/qembed-object-runtime/actions/runs/44811246397), exact HEAD `1304b9c99096c7251c43edaca6623ab933d05db0`; PostgreSQL 15.20/16.16/19.0 + pgvector 0.8.7; 212/212 workspace tests passed per version; Rust 1.85 MSRV passed. Artifacts are linked in `docs/RUST-049_M1A_TEST_EVIDENCE.md`.
  - DoD: JUnit/nextest reports, DB/pgvector metadata, full logs and concurrency results are reproducible from CI artifacts.

- [x] **RUST-050 (XS)** Зафиксировать M1a completion и обновить roadmap.
  - Release note: [M1a release notes](docs/M1A_RELEASE_NOTES.md) перечисляет закрытые задачи RUST-045…049, acceptance/CI evidence, известные ограничения и границы defect review.
  - Release gate: полный CI workflow должен быть зелёным на точном RUST-050 release commit; SHA и run зафиксированы в PR #73.
  - DoD: применимые M1a acceptance cases и обязательные Rust/DB suites подтверждены; M2 и production-lifecycle ограничения явно сохранены.

**M1a Definition of Done:** walking skeleton работает end-to-end; все применимые M1a acceptance tests зелёные; unit, integration, DB integration и compile-fail suites проходят; нет открытых Critical/High дефектов в реализованном Rust path без formal waiver.

---

## 9. Этап M1b — JetStream delivery, durable workers, budgets и cancellation

**Результат:** надёжная асинхронная обработка Invocation через NATS JetStream при сохранении PostgreSQL единственным источником истины. Внешние write-tools остаются выключенными до M3 gate.

**Архитектурное решение:** [ADR RT-048 — NATS JetStream transport with PostgreSQL source of truth](docs/ADR-RT-048_NATS_JETSTREAM_TRANSPORT.md).

### 9.1. Outbox → JetStream

- [ ] **RUST-051 (L)** Реализовать PostgreSQL Outbox Relay для NATS JetStream. Задача разделена на независимые проверяемые части:
  - [x] **RUST-051a (M) — PostgreSQL claim primitive.** Реализовано и слито: Rust PR #74 (`1d38cc4e`) + каноническая SQL-схема PR #20 (`977f9152`). Restricted delivery projection, короткая транзакция claim через FOR UPDATE SKIP LOCKED, lease owner/expiry, monotonic fencing token, attempts, fenced acknowledge/release, expiry reclaim и PostgreSQL tests на stale ack, competing relays и role isolation. Это только SQL-часть relay, не готовый NATS publisher и не завершённая RUST-051.
  - [ ] **RUST-051b (M) — JetStream publisher adapter.** Добавить broker-neutral MessagePublisher и OutboxDeliveryStore ports, Relay orchestration и concrete async-nats JetStream adapter в composition root; публиковать только committed Outbox deliveries; использовать стабильный delivery_id как Nats-Msg-Id; ждать PubAck; фиксировать latest stream/sequence/duplicate receipt и append-only историю PubAck в PostgreSQL; сетевой I/O выполнять вне SQL-транзакции. Зафиксировать async-nats 0.53.0 (crate MSRV 1.89.0, совместим с workspace Rust 1.100) в Cargo.toml и Cargo.lock.
  - [ ] **RUST-051c (M) — Relay failure semantics.** Проверить crash после claim/до publish, после publish/до PubAck, после PubAck/до PostgreSQL update, NATS outage/reconnect и параллельные relay processes. Повторная публикация сохраняет тот же logical delivery_id; bounded JetStream deduplication не заменяет consumer idempotency.
  - Тесты: PostgreSQL integration + реальный JetStream test server; проверка PubAck, redelivery, stable message identity, lease fencing и отсутствия сетевого I/O внутри SQL-транзакции.
  - DoD: Outbox Relay публикует committed messages в JetStream at-least-once; PostgreSQL остаётся источником восстановления; RUST-051a без RUST-051b не считается завершённой RUST-051.

### 9.2. Durable consumers, retry и recovery

- [ ] **RUST-052 (L)** Реализовать durable JetStream consumer lifecycle, retry policy и PostgreSQL-driven recovery. Задача разделена на:
  - [ ] **RUST-052a (M) — Durable pull consumer.** Реализовать broker-neutral consumer port и JetStream durable pull consumer с explicit ACK; message ACK разрешён только после commit результата/состояния в PostgreSQL. На повторной доставке Worker сначала проверяет Invocation/ToolExecution state и leases/fencing.
  - [ ] **RUST-052b (M) — Retry/backoff и poison-message policy.** Разделить transient transport/worker failures, permanent invalid envelopes и business-terminal/Unknown outcomes. Настроить bounded backoff, max delivery safety limit и DLQ/parking stream; бизнес retry state и причина terminal отказа фиксируются в PostgreSQL. DLQ не является источником истины.
  - [ ] **RUST-052c (M) — PostgreSQL recovery sweeper и durable replay marker.** Находит runnable/queued Invocation без активного lease и с просроченным progress deadline; проверяет terminal/Unknown state; выставляет durable replay-request marker в PostgreSQL для исходной Outbox row; Relay повторно публикует тот же payload с тем же delivery_id, не стирая историю PubAck. Не делает auto-retry для Unknown и не обходит provider reconciliation.
  - Тесты: deterministic clock; JetStream integration на ack/redelivery/consumer restart; PostgreSQL recovery integration; повтор после commit до ACK; broker message loss/replay simulation; poison event; max attempts; no duplicate business side effect.
  - DoD: сообщение подтверждается только после DB commit; потерянная доставка восстанавливается из PostgreSQL; retries не обходят Invocation/ToolExecution state machine, budgets, cancellation, idempotency и reconciliation.

### 9.3. Execution safety и budgets

- [x] **RUST-053 (M)** Реализовать независимую ToolExecution state machine и lease.
  - Тесты: unit transition matrix; DB integration на параллельные leases.
  - DoD: invocation lease и ToolExecution lease не используют общие token/version.

- [x] **RUST-054 (M)** Реализовать Unknown transition после неоднозначного execution timeout.
  - Тесты: unit и DB integration — timeout, expired lease, запрет auto-retry, попытка запрещённого перехода.
  - DoD: Unknown не переходит в повторное выполнение автоматически.

- [x] **RUST-055 (M)** Реализовать reconciliation port и workflow.
  - Тесты: mock reconciler для confirmed success, confirmed failure и unresolved outcome; stale fencing token rejected.
  - DoD: terminal outcome фиксируется только через актуальный reconciliation result.

- [x] **RUST-056 (M)** Реализовать стабильный command_id = UUIDv5(invocation_id, step_index, action_hash).
  - Реализация: invocation UUID используется как UUIDv5 namespace; имя состоит из `qembed.command-id.v1\0`, `step_index` (`u32`, big-endian) и 32-байтового SHA-256 `action_hash`. `AllowedCommand` получает ID только через этот генератор.
  - Тесты: фиксированный UUIDv5 vector; восстановление входов после restart; различие ID для разных invocation, step и action.
  - DoD: один и тот же logical command получает одинаковый ID после рестарта.

- [x] **RUST-057 (M)** Реализовать и проверить конкурентные budget reservations.
  - Сериализация: каждая reservation блокирует tenant/object row в `object_budgets` через `FOR UPDATE` до проверки aggregate usage, caller usage и concurrency ceiling; счётчики и reservation записываются в той же транзакции.
  - Тесты: две независимые PostgreSQL sessions одновременно резервируют один оставшийся daily budget — ровно одна транзакция проходит; конкурентный одинаковый idempotency key даёт один Created и один Replay; downstream failure после reservation откатывает reservation и оба счётчика.
  - DoD: совокупный consumed + reserved usage не превышает effective object/caller budget; replay не создаёт вторую reservation.

- [x] **RUST-058 (S)** Реализовать distributed rate limiting и pool isolation.
  - SQL dependencies: QSA-003 qctx2 verifier/accessors merged in `qembed-sql` PR #83; RT-060 Object-scoped Anonymous Caller provisioning merged in PR #85. These migrations are prerequisites for the Rust Anonymous transaction path.
  - Старый Rust PR #90 признан устаревшим: он расходился с актуальным `main`, подключал только Owner, строил Authenticated key по Caller и списывал token до разрешения idempotency. Не продолжать эту ветку; реализация начинается от актуального `main`.
  - Разбиение: RUST-058a — domain/runtime contracts и trusted key provenance; RUST-058b — Valkey atomic Token Bucket adapter и independent-connection concurrency; RUST-058c — SQL-backed Principal/Owner invocation identity, Owner и Principal HTTP admission classes и replay-aware idempotency ordering; RUST-058d — signed qctx2 Anonymous context, Object-scoped Caller, server-derived fingerprint, public HTTP route, fingerprint-isolated rate/idempotency admission, structured observability и acceptance suite.
  - Архитектура и границы: `docs/ADR-RUST-058-DISTRIBUTED-RATE-LIMITING.md`. PostgreSQL `InvocationPool`/token-budget reservations остаются durable accounting; Valkey хранит только ephemeral request-throttling state. Anonymous не получает synthetic Principal; Caller стабилен на уровне Object, а fingerprint изолирует конкретного peer для rate limit и idempotency.
  - DoD: все три класса подключены к invocation surface; Authenticated quota привязана к trusted Principal, не Caller; Anonymous не принимает Principal/Caller ID из DTO; completed/concurrent idempotent replay не списывает повторный token; Valkey outage остаётся fail-open без локального fallback и наблюдаем через structured telemetry; independent Valkey workers подтверждают общий bucket state; PostgreSQL 19/HTTP/Valkey CI green.

- [x] **RUST-059 (M)** Реализовать hierarchical kill-switch evaluation.
  - Тесты: unit на precedence Platform/Tenant/Object/Tool; integration — disabled scope блокирует действие до side effect.
  - DoD: проверка выполняется непосредственно перед разрешённым execution path.

- [x] **RUST-060 (M)** Реализовать cancellation propagation в worker orchestration.
  - Тесты: integration — cancel до старта, во время ожидания, между шагами; после принятой отмены новые tool calls не стартуют.
  - DoD: cancellation распространяется на все последующие шаги.

- [x] **RUST-061 (M)** Реализовать обязательный External Caller Deletion Ledger и restore replay. Локального ledger/fallback режима нет.
  - Архитектура: `docs/ADR-RUST-061-EXTERNAL-CALLER-DELETION-LEDGER.md`. Control-plane PostgreSQL — отдельный кластер и единственный authoritative источник append-only HMAC tombstones; `CONTROL_PLANE_DATABASE_URL` и v1 HMAC secret обязательны.
  - Протокол: Runtime сначала отзывает Caller (без физического удаления), затем фиксирует tombstone в Control-plane, после durable acknowledgement очищает Caller-derived data. Ошибки оставляют Caller revoked и deletion job retryable; local fallback запрещён.
  - Restore: readiness/traffic закрыты до полного replay внешнего ledger, проверки отсутствия Caller-derived rows и достижения стабильного high-water mark. Для replay используется узко привилегированная cross-tenant capability без ослабления обычного RLS.
  - Evidence: SQL PR [#94](https://github.com/qembed/qembed-sql/pull/94) и Rust PR [#134](https://github.com/qembed/qembed-object-runtime/pull/134); Rust CI run #2900 и canonical SQL smoke прошли.
  - DoD: erased Caller data не возвращаются после restore; Control-plane недоступность не приводит к downgrade или открытию traffic.

- [x] **RUST-062 (S)** Реализовать worker-only reservation sweeper.
  - Rust adapter: `qembed_store::reservation_sweeper::PgReservationSweeper` использует отдельный pool с login, наследующим restricted роль `qembed_worker`; операция глобальная, без tenant Scope.
  - Eligibility: истёкшие `Reserved` rows освобождаются только при provider attempt `NotStarted`, `ConfirmedNotAccepted` или `CancelledBeforeSend`. `MayHaveBeenSent`, `OutcomeUnknown`, `ResponseReceived` и отсутствующая attempt row сохраняют reservation.
  - SQL: детерминированный `p_now` cutoff, default на PostgreSQL clock, batch 1..=500; NULL и невалидные batch sizes отклоняются. Provider-attempt row lock с `SKIP LOCKED` сериализует конкурирующие sweepers; reservation и object/per-caller counters меняются атомарно.
  - Тесты: deterministic cutoff, повторный запуск, два независимых worker pools, preservation of OutcomeUnknown и проверка стабильности object/per-caller counters после повторного запуска.
  - Evidence/design: `docs/RUST-062_RESERVATION_SWEEPER.md`; SQL PRs [#95](https://github.com/qembed/qembed-sql/pull/95) и [#96](https://github.com/qembed/qembed-sql/pull/96); Rust PR [#135](https://github.com/qembed/qembed-object-runtime/pull/135), CI run [#2911](https://github.com/qembed/qembed-object-runtime/actions/runs/45478611692).
  - DoD: sweeper идемпотентен и не освобождает reservation при неоднозначном provider outcome.

- [x] **RUST-063 (M)** Добавить concurrency suite на независимых PostgreSQL sessions и relay/consumer instances.
  - Тесты: stale CAS, lease reclaim, concurrent budget reservation, competing state transition, competing relay claims и duplicate JetStream deliveries.
  - DoD: тесты запускаются в CI и воспроизводят гонки на независимых PostgreSQL sessions и реальном JetStream, а не используют один connection или broker mocks.
  - Evidence: Rust PR [#136](https://github.com/qembed/qembed-object-runtime/pull/136), merge `aa582a9`; CI run [#2921](https://github.com/qembed/qembed-object-runtime/actions/runs/45480902051) — все jobs зелёные, включая PostgreSQL + JetStream workspace tests.

- [x] **RUST-064 (S)** Добавить M1b failure-injection matrix.
  - Тесты: crash до NATS connect, после claim до publish, после publish до PubAck, после PubAck до PostgreSQL publication receipt, после DB commit до JetStream ACK, broker message loss до/после consumer delivery, worker restart и PostgreSQL recovery replay.
  - DoD: для каждого crash point зафиксирован ожидаемый PostgreSQL state, broker state и доказуемый recovery path; Unknown не переисполняется автоматически.
  - Evidence: Rust PR [#137](https://github.com/qembed/qembed-object-runtime/pull/137), merge `0abd208`; CI run [#2941](https://github.com/qembed/qembed-object-runtime/actions/runs/45486649589) — все jobs зелёные, включая destructive NATS storage-loss recovery test.

**M1b Gate:** Outbox → JetStream publication, durable pull consumers, explicit ACK-after-commit, idempotent redelivery, PostgreSQL-driven recovery, leases/fencing, cancellation, budgets, rate limits и failure-injection matrix подтверждены автоматическими PostgreSQL + JetStream tests. NATS availability/loss не может уничтожить единственную копию задания или бизнес-состояния.

## 10. Этап M2 — Knowledge, retrieval и verification

**Результат:** ingestion и retrieval по закреплённой revision с проверяемым provenance.

- [x] **RUST-065 (M)** Определить domain model для KnowledgeDocument, Chunk, Revision и pinned revision.
  - Реализация: typed document/chunk IDs; tenant+object-scoped current/pinned revisions; half-open visibility intervals; guarded document lifecycle и terminal erasure; immutable chunk content с erasure-only mutation; invariant-validating deserialization.
  - Тесты: unit state transition matrix; serialization round-trip; invalid revision references; historical visibility after retirement; chunk visibility/erasure.
  - Evidence: Rust PR [#138](https://github.com/qembed/qembed-object-runtime/pull/138), merge `034ed67353b9563a09d779de592ab7722676612f`; CI run [#2952](https://github.com/qembed/qembed-object-runtime/actions/runs/45491358354) — success.
  - DoD: staged/indexed/live/retired/erased lifecycle и tenant/object-scoped revision identity типизированы; persistence и atomic revision commit остаются Store responsibilities в RUST-066.

- [ ] **RUST-066 (L)** Реализовать `commit_knowledge_revision` repository operation.  - Тесты: DB integration — успешный commit, rollback, concurrent commit, revision log, event/outbox atomicity.
  - DoD: lock revision row; проверить indexing completeness; обновить intervals/revision/event в одной транзакции.
- [ ] **RUST-067 (M)** Реализовать staged ingestion/indexing pipeline.
  - Тесты: unit на chunking/validation; integration — staged chunks не видны retrieval до commit; повтор ingestion идемпотентен.
  - DoD: неполный документ не становится видимым.

- [ ] **RUST-068 (M)** Реализовать embedding model registry.
  - Тесты: contract tests для размерности/метрики; negative tests на смешение несовместимых embeddings.
  - DoD: разные модели и размерности изолированы.

- [ ] **RUST-069 (M)** Реализовать retrieval filtering по revision до vector top-k.
  - Тесты: DB integration с staged, future, closed и pinned revisions; regression test на порядок filter/top-k.
  - DoD: нерелевантные revision rows исключаются до top-k.

- [ ] **RUST-070 (M)** Реализовать pgvector query paths.
  - Тесты: integration на IP/L2/cosine; проверка порядка результатов и размерностей.
  - DoD: IP similarity = `-dist`; `1-dist` применяется только к cosine.
- [ ] **RUST-071 (M)** Реализовать HNSW index migration support.
  - Тесты: migration integration — `CREATE INDEX CONCURRENTLY` вне транзакции; повторный запуск и recovery после неудачи.
  - DoD: migrator корректно управляет non-transactional index operations.

- [ ] **RUST-072 (M)** Реализовать retrieval pinning и GC protection.
  - Тесты: DB integration — active invocation pins revision; GC не удаляет pinned revision; release pin после завершения.
  - DoD: активный invocation воспроизводимо читает зафиксированную revision.

- [ ] **RUST-073 (M)** Реализовать citation/provenance verifier V1–V7.
  - Тесты: positive/negative corpus для chunk IDs, чисел, ссылок и обязательных citations; property tests на malformed claims.
  - DoD: verifier отклоняет неподтверждённые claims.

- [ ] **RUST-074 (M)** Реализовать application-level provenance и caller isolation.
  - Тесты: integration — Caller A не читает episodic memory Caller B; forged provenance rejected.
  - DoD: provenance проверяется на app path, а не только DB constraints.

- [ ] **RUST-075 (S)** Реализовать memory promotion как новую Owner-authored Verified row.
  - Тесты: domain и DB integration — candidate остаётся неизменным; только TenantOwner может подтвердить; supersedes chain валидна.
  - DoD: promotion создаёт новую Verified запись с корректной ссылкой на superseded candidate.

- [ ] **RUST-076 (S)** Добавить knowledge concurrency и GC suite.
  - Тесты: commit race, pinned invocation, GC race, rollback и повторная публикация.
  - DoD: suite запускается на PostgreSQL в CI.

**M2 Gate:** revision commit, retrieval filtering, pinning/GC и provenance verifier покрыты unit и PostgreSQL integration tests.

---

## 11. Этап M3 — Policy, tools и SSRF-safe execution

**Результат:** исполнение только явно разрешённых команд с защитой от SSRF и неоднозначных side effects.

- [ ] **RUST-077 (M)** Реализовать typed policy evaluation contract.
  - Тесты: decision matrix; adversarial input tests; недоверенный model/document content не может повысить verdict.
  - DoD: policy decision — типизированный результат с audit context.

- [ ] **RUST-078 (M)** Реализовать Authorizer и единственную mint boundary `AllowedCommand`.
  - Тесты: unit — allow/deny; compile-fail — executor не принимает raw action; negative tests на forged command.
  - DoD: executor принимает только capability, а не ProposedAction/raw JSON/tool ID/raw URL.

- [ ] **RUST-079 (M)** Реализовать tool registry adapter и immutable ToolVersion resolution.
  - Тесты: contract/integration — snapshot разрешает точную версию; отсутствующая/несовпадающая версия rejected.
  - DoD: contract tool не меняется под существующим ToolVersion ID.

- [ ] **RUST-080 (M)** Реализовать binding validation и stale capability rejection.
  - Тесты: DB integration — binding изменён после mint; execution блокируется до network call.
  - DoD: capability связана с актуальной binding identity/version.

- [ ] **RUST-081 (M)** Реализовать DNS resolver с проверкой полного A+AAAA RRset.
  - Тесты: deterministic resolver fixtures; mixed public/private, empty, malformed и special answers.
  - DoD: весь RRset проверяется; mixed public/private отклоняется.

- [ ] **RUST-082 (L)** Реализовать pinned connect к проверенному IP.
  - Тесты: network integration с контролируемым DNS; DNS rebinding test; TLS SNI/hostname verification.
  - DoD: соединение идёт к проверенному IP при сохранении корректной TLS identity.

- [ ] **RUST-083 (M)** Реализовать безопасную redirect policy.
  - Тесты: redirect chain на public→private, cross-host, loop и превышение лимита.
  - DoD: redirect либо запрещён, либо каждый hop проходит полную повторную валидацию.

- [ ] **RUST-084 (S)** Изолировать proxy environment и egress policy.
  - Тесты: тесты с HTTP_PROXY/HTTPS_PROXY/ALL_PROXY; проверка фактического destination.
  - DoD: не происходит непреднамеренного proxy routing.

- [ ] **RUST-085 (M)** Добавить IPv4/IPv6 special-range validation.
  - Тесты: corpus для mapped IPv6, NAT64, CGNAT, link-local, loopback, multicast и metadata IPs.
  - DoD: запрещённые диапазоны отвергаются.

- [ ] **RUST-086 (M)** Реализовать envelope encryption для command payload store.
  - Тесты: encrypt/decrypt round-trip; tamper/hash mismatch; wrong key; key rotation; ciphertext-only persistence assertion.
  - DoD: payload зашифрован at rest; key ID поддерживает rotation; hash проверяется до execution.

- [ ] **RUST-087 (S)** Реализовать terminal payload cleanup и Unknown retention behavior.
  - Тесты: integration — terminal cleanup, TTL, Unknown сохранён до reconciliation/TTL, повторный cleanup.
  - DoD: cleanup идемпотентен и не удаляет payload, нужный для reconciliation.

- [ ] **RUST-088 (M)** Реализовать ToolExecution executor и upstream idempotency propagation.
  - Тесты: mock upstream — стабильный idempotency key после restart; timeout ambiguity; отсутствие auto-retry для non-idempotent action.
  - DoD: side effect не повторяется без доказанной idempotency/reconciliation.

- [ ] **RUST-089 (M)** Реализовать reconciliation adapters для конкретных upstream providers.
  - Тесты: provider contract tests для confirmed success/failure/unknown; stale token rejected.
  - DoD: неизвестный outcome не превращается в успех/ошибку без подтверждения.
- [ ] **RUST-090 (M)** Добавить tool failure-injection suite.
  - Тесты: все crash points из acceptance plan; повторный запуск; stale worker; network timeout.
  - DoD: состояние после каждого crash point проверяется автоматически.

- [ ] **RUST-091 (S)** Проверить event/log/API payload hygiene.
  - Тесты: capture logs/events/responses; assertions на отсутствие secrets, raw command payload и sensitive arguments.
  - DoD: секреты и raw payload не покидают encrypted payload boundary.

- [ ] **RUST-092 (S)** Подготовить automated tool-enablement test gate.
  - Тесты: единый CI job запускает SSRF, capability, binding, lease, Unknown, reconciliation и payload security suites.
  - DoD: write-tools нельзя включить конфигурацией, если обязательный test gate не прошёл.

**M3 Gate:** SSRF, pinned connect, capability, binding, independent leases, Unknown/reconciliation и encrypted payload подтверждены автоматическими тестами. До этого внешние write-tools остаются выключенными.

---

## 12. Этап M4 — Tasks и checkpoints

**Результат:** durable multi-step tasks поверх проверенных invocation/worker primitives.

- [ ] **RUST-093 (S)** Уточнить domain contracts Task и Checkpoint.
  - Тесты: state transition matrix; invalid checkpoint sequence; serialization round-trip.
  - DoD: task/checkpoint invariants описаны и проверяются до persistence.

- [ ] **RUST-094 (M)** Реализовать Task/Checkpoint repositories.
  - Тесты: DB integration — create/update/read, tenant isolation, invalid transitions, rollback.
  - DoD: repository сохраняет только допустимые transitions.

- [ ] **RUST-095 (M)** Реализовать scheduler/worker integration.
  - Тесты: integration — scheduling, claim, lease expiry, competing workers.
  - DoD: task продолжает работу только с последнего валидного checkpoint.

- [ ] **RUST-096 (M)** Реализовать idempotent resume и crash recovery.
  - Тесты: failure injection между шагами; repeated resume; no duplicate completed step.
  - DoD: подтверждённые шаги не исполняются повторно.

- [ ] **RUST-097 (S)** Добавить M4 acceptance suite.
  - Тесты: task/checkpoint durability, concurrency и restore cases; соответствующий acceptance test T-26.
  - DoD: тестовый отчёт прикладывается к M4 release evidence.

---

## 13. Сквозные тестовые требования

Следующие тесты должны быть не разовыми, а постоянной частью CI:

1. **Domain unit tests:** state machines, validation, budget calculations, policy contracts.
2. **Compile-fail tests:** `AllowedCommand`, запрещённые зависимости и недоступные конструкторы.
3. **Store integration tests:** PostgreSQL, реальные роли, RLS, tenant isolation, FK, append-only.
4. **Concurrency tests:** минимум два независимых DB connection/session для CAS, leases, budgets и revision commit.
5. **Provider contract tests:** mock provider на M1a; реальные adapter contract tests при добавлении provider.
6. **Network security tests:** DNS mixed RRset, rebinding, redirect, IPv4/IPv6 special ranges, proxy bypass.
7. **Failure injection:** crash/timeout на каждой границе persistence и внешнего side effect.
8. **API integration tests:** auth, DTO validation, tenant isolation, error mapping и response minimization.
9. **End-to-end:** M1a walking skeleton и последующие milestone flows.
10. **Regression:** каждый найденный дефект получает тест, который падает до исправления и остаётся в CI после него.

### Правило закрытия каждой задачи

Задача не переводится в Done, пока не выполнены все применимые пункты:

- [ ] Реализация завершена и находится в правильном crate.
- [ ] Добавлены unit tests для локальной логики.
- [ ] Добавлены integration/DB/API tests для затронутой границы.
- [ ] Добавлены negative/security tests для отказных сценариев.
- [ ] Все тесты задачи проходят локально и в CI.
- [ ] `cargo fmt --check` и Clippy проходят.
- [ ] SQLx metadata обновлена и проверена, если затронуты SQLx queries.
- [ ] Документация и acceptance mapping обновлены при изменении контракта.

---

## 14. Рекомендуемый порядок первых pull requests

PR должны оставаться небольшими. Если один пункт разрастается, разделить его, сохранив task ID и добавив суффиксы.

1. **PR-01:** RUST-001…009 — workspace, CI, PostgreSQL harness, SQLx offline.
2. **PR-02:** RUST-010…013 — IDs, Scope, Principal, entities и ports.
3. **PR-03:** RUST-014…015 — `AllowedCommand` и compile-fail tests.
4. **PR-04:** RUST-016…018 — transaction wrapper, tenant context, auth/principal repositories.
5. **PR-05:** RUST-019…023 — Object/version/publish/snapshot/JCS persistence.
6. **PR-06:** RUST-024…029 — events/outbox, CAS, RLS и role integration tests.
7. **PR-07:** RUST-030…034 — ChatProvider contract и deterministic mock.
8. **PR-08:** RUST-035…043 — idempotency, invocation worker, leases, budget, errors, health.
9. **PR-09a:** RUST-044-T — Axum + Tower migration; [ADR-RT-053](docs/ADR-RT-053-AXUM-TOWER-HTTP-TRANSPORT.md). Выполнено.
10. **PR-09b:** RUST-044 — Owner API endpoints, API integration coverage и PostgreSQL/security DoD. Выполнено.
12. Затем M1b → M2 → M3 → M4.

Не начинать M3 tool execution только потому, что M1a API/worker skeleton уже работает.

---

## 15. Что не входит в этот roadmap

- Исправление, аудит и версионирование SQL-схемы как самостоятельная работа.
- Проектирование/изменение архитектуры M0 и добавление новых crates.
- Tenant offboarding/erasure schema design, legal review и production operations.
- Infrastructure-as-code, production deployment, SRE runbooks и compliance sign-off.
- UI/frontend.

При этом Rust-код тестируется против PostgreSQL как реального persistence backend. Исключение SQL remediation из roadmap не означает, что DB integration tests можно заменить mock-ами или пропустить.
---

## 16. Source of truth

Roadmap декомпозирует Rust-часть материалов Qembed Object Runtime M0 v2.3.3 и решений RT-039…RT-047. Нумерация `RUST-*`, оценки, порядок PR и тестовые подзадачи — рабочая декомпозиция, а не новые ADR.

**Стартовая точка:** E0 → M1a Domain → M1a Store → Mock ChatProvider → Invocation/API walking skeleton. После каждого отдельного шага обязательны тесты, включая негативные и интеграционные там, где затрагиваются границы безопасности или persistence.