# Qembed Object Runtime — Rust Development Roadmap

> **Файл:** `ROADMAP.md`  
> **База:** Qembed Object Runtime M0 v2.3.3; ADR RT-039…RT-047  
> **Фокус:** только Rust-разработка и тестирование Rust-кода  
> **Первый milestone:** M1a — walking skeleton `create → publish → invoke → retrieve result` на mock ChatProvider.

---

## 1. Принципы разработки

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
| `qembed-domain` | IDs, entities, `Scope`, `Principal`, state machines, policy contracts, ports, `AllowedCommand` | Без SQLx, Actix, provider SDK и сетевого I/O |
| `qembed-store` | SQLx repositories, transaction context, persistence, CAS/fencing | Без HTTP handlers и provider-specific logic |
| `qembed-llm` | ChatProvider adapters, capability negotiation, structured output, usage/error mapping | Без orchestration и tool authorization |
| `qembed-tools` | Tool registry adapter, executor/reconciler, SSRF-safe networking, encrypted payload handling | Не принимает policy verdict самостоятельно |
| `qembed-runtime` | Orchestration, budgets, invocation/worker lifecycle, retrieval coordination | Без HTTP transport и SQL query details |
| `qembed-api` | Actix handlers, auth boundary, DTO/OpenAPI, composition root | Handler-ы не содержат бизнес-правила |

**Dependency rule:** domain не зависит от adapters; adapters реализуют domain ports; runtime координирует domain и ports; API handlers вызывают runtime. Package/binary `qembed-api` может зависеть от concrete adapter crates только в composition root для сборки приложения. Handler modules ограничиваются `qembed-domain` и `qembed-runtime`. Не добавлять седьмой crate.

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
  - DoD: domain не зависит от SQLx, Actix или provider SDK.

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

- [ ] **RUST-029 (S)** Добавить тесты role separation для store integration.
  - Реализация: отдельные NOLOGIN group roles runtime/dispatcher/admin; реальные LOGIN test identities; RLS на всех tenant-owned таблицах; транзакционный tenant context для pre-auth owner lookup.
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

- [ ] **RUST-036 (M, implementation in progress)** Реализовать атомарный begin-invocation transaction и SQL-011 Principal/Caller delegation integration.
  - Реализация: отдельные PrincipalId и CallerId; transaction-local app.tenant_id + app.principal_id; SQL-011 resolve_delegated_caller с блокировками; idempotency scoped по ResolvedCaller; explicit InvocationPool; атомарные snapshot/revision pin, invocation insert, critical event и outbox.
  - Тесты: PostgreSQL integration — commit/replay, caller scoping, invalid snapshot rollback, injected failures at invocation/event/outbox writes. SQL clean-install/smoke validation remains a separate qembed-sql gate.
  - Документация: PRINCIPAL_CALLER_DELEGATION.md, BEGIN_INVOCATION_TRANSACTION.md, idempotency contract and Store role matrix.
  - DoD: либо фиксируется весь begin-invocation, либо ничего; provider I/O начинается только после commit.

- [ ] **RUST-037 (M)** Реализовать минимальный invocation worker lifecycle.
  - Тесты: integration — claim/process/finish; restart/resume; terminal result не теряется.
  - DoD: invocation lifecycle сохраняется в PostgreSQL и переживает рестарт процесса.

- [ ] **RUST-038 (M)** Реализовать invocation lease и fencing-token CAS.
  - Тесты: две независимые worker sessions; lease expiry/reclaim; старый worker не может записать результат после выдачи нового token.
  - DoD: stale worker не завершает invocation.

- [ ] **RUST-039 (S)** Реализовать `GET invocation`.
  - Тесты: API integration — queued/running/succeeded/failed; tenant isolation; скрытие внутренних payloads.
  - DoD: клиент получает только разрешённый status/result/error.

- [ ] **RUST-040 (S)** Реализовать cancellation contract.
  - Тесты: unit на state transitions; API integration на cancel, repeated cancel и terminal invocation.
  - DoD: cancellation persisted; API не обещает отмену уже совершённого внешнего side effect.

- [ ] **RUST-041 (M)** Реализовать минимальный budget reservation/settlement для mock path.
  - Тесты: unit на вычисление effective budget; DB integration на reserve/settle, повтор и rollback.
  - DoD: effective budget = `min(snapshot limit, live object budget)`; reservation не дублируется при idempotent повторе.

- [ ] **RUST-042 (S)** Реализовать typed runtime errors и API error mapping.
  - Тесты: unit на все error mappings; API tests на стабильные error codes и отсутствие SQL/provider internals.
  - DoD: ошибки пригодны для клиента и не раскрывают внутренние детали.

- [ ] **RUST-043 (S)** Реализовать health/readiness endpoints.
  - Тесты: API integration — healthy DB, unavailable DB, migration mismatch; проверка отсутствия secrets в response.
  - DoD: readiness проверяет обязательные зависимости.

- [ ] **RUST-044 (M)** Собрать Owner API: create object/version, publish, invoke, inspect invocation.
  - Тесты: API integration на каждый endpoint; auth/tenant negative cases; DTO validation.
  - DoD: handler-ы только валидируют transport input и вызывают runtime use cases.

- [ ] **RUST-045 (M)** Добавить полный walking-skeleton end-to-end test.
  - Тесты: create → publish → invoke → retrieve result; duplicate idempotency key; process restart; cross-tenant denied.
  - DoD: flow проходит через HTTP/API, runtime, PostgreSQL и Mock ChatProvider без внешнего сервиса.
- [ ] **RUST-046 (S)** Проверить DB-backed provenance constraints, относящиеся к M1a.
  - Тесты: integration — запрещённые provenance claims отвергаются; app-level verifier остаётся тестом M2.
  - DoD: тесты не заявляют наличие application-level retrieval/provenance до реализации M2.

- [ ] **RUST-047 (S)** Добавить failure-injection tests для M1a crash boundaries.
  - Тесты: crash до/после transaction commit; provider timeout; restart worker; outbox rollback.
  - DoD: доказана атомарность persistence и restart durability mock-only flow.

- [ ] **RUST-048 (M)** Прогнать M1a acceptance suite.
  - Тесты: все M1a cases из `acceptance_T-01_to_T-64.md`, включая tenant isolation, idempotency, snapshots, events, API и capability compile-fail.
  - DoD: тестовый отчёт содержит результат каждого применимого M1a test ID.

- [ ] **RUST-049 (S)** Подготовить M1a test evidence.
  - Тесты: CI публикует JUnit/nextest report, DB version, pgvector version, test logs и concurrency test results.
  - DoD: результаты воспроизводимы из CI artifacts.

- [ ] **RUST-050 (XS)** Зафиксировать M1a completion и обновить roadmap.
  - Тесты: повторный полный CI run на release commit.
  - DoD: закрытые тесты и известные ограничения перечислены в release note.

**M1a Definition of Done:** walking skeleton работает end-to-end; все применимые M1a acceptance tests зелёные; unit, integration, DB integration и compile-fail suites проходят; нет открытых Critical/High дефектов в реализованном Rust path без formal waiver.

---

## 9. Этап M1b — Durable workers, outbox, budgets и cancellation

**Результат:** надёжная обработка конкурентных и длительных invocation. Внешние write-tools остаются выключенными до M3 gate.

- [ ] **RUST-051 (M)** Реализовать outbox dispatcher.
  - Тесты: DB integration — claim, delivery, ack; dispatcher видит только delivery projection; consumer deduplicates.
  - DoD: доставка at-least-once с безопасной дедупликацией.

- [ ] **RUST-052 (M)** Реализовать retry/backoff и dead-letter handling.
  - Тесты: deterministic clock tests; integration — повтор, max attempts, poison event, recovery.
  - DoD: retry не теряет событие и не создаёт повторный side effect у consumer.

- [ ] **RUST-053 (M)** Реализовать независимую ToolExecution state machine и lease.
  - Тесты: unit transition matrix; DB integration на параллельные leases.
  - DoD: invocation lease и ToolExecution lease не используют общие token/version.

- [ ] **RUST-054 (M)** Реализовать `Unknown` transition после неоднозначного execution timeout.
  - Тесты: unit и DB integration — timeout, expired lease, запрет auto-retry, попытка запрещённого перехода.
  - DoD: `Unknown` не переходит в повторное выполнение автоматически.

- [ ] **RUST-055 (M)** Реализовать reconciliation port и workflow.
  - Тесты: mock reconciler для confirmed success, confirmed failure и unresolved outcome; stale fencing token rejected.
  - DoD: terminal outcome фиксируется только через актуальный reconciliation result.

- [ ] **RUST-056 (M)** Реализовать стабильный `command_id = UUIDv5(invocation_id, step_index, action_hash)`.
  - Тесты: deterministic vectors; restart test; uniqueness tests.
  - DoD: один и тот же logical command получает одинаковый ID после рестарта.

- [ ] **RUST-057 (M)** Реализовать конкурентные budget reservations.
  - Тесты: DB integration с параллельными транзакциями; overspend race; rollback; duplicate idempotency.
  - DoD: совокупный reserve не превышает effective budget.

- [ ] **RUST-058 (S)** Реализовать distributed rate limiting и pool isolation.
  - Тесты: concurrency tests на несколько workers; fairness tests для Anonymous/Authenticated/Owner.
  - DoD: лимиты согласованы между экземплярами runtime.

- [ ] **RUST-059 (M)** Реализовать hierarchical kill-switch evaluation.
  - Тесты: unit на precedence Platform/Tenant/Object/Tool; integration — disabled scope блокирует действие до side effect.
  - DoD: проверка выполняется непосредственно перед разрешённым execution path.

- [ ] **RUST-060 (M)** Реализовать cancellation propagation в worker orchestration.
  - Тесты: integration — cancel до старта, во время ожидания, между шагами; после принятой отмены новые tool calls не стартуют.
  - DoD: cancellation распространяется на все последующие шаги.

- [ ] **RUST-061 (M)** Реализовать caller deletion ledger и restore replay.
  - Тесты: integration — delete, повторный delete, restore replay, повторное удаление caller-derived data.
  - DoD: erased caller data не восстанавливаются при restore.

- [ ] **RUST-062 (S)** Реализовать reservation sweeper.
  - Тесты: deterministic clock; повторный запуск; параллельные sweeper; counters не обновляются дважды.
  - DoD: sweeper идемпотентен.

- [ ] **RUST-063 (M)** Добавить concurrency suite на независимых PostgreSQL sessions.
  - Тесты: stale CAS, lease reclaim, concurrent budget reservation, competing state transition.
  - DoD: тесты запускаются в CI и воспроизводят гонки, а не используют один connection.

- [ ] **RUST-064 (S)** Добавить M1b failure-injection matrix.
  - Тесты: crash до connect, после send до response, после response до DB commit, после event insert до outbox ack.
  - DoD: для каждого crash point зафиксирован ожидаемый state и проверяемое восстановление.

**M1b Gate:** outbox redelivery, leases/fencing, cancellation, budgets, rate limits и recovery подтверждены автоматическими тестами.

---

## 10. Этап M2 — Knowledge, retrieval и verification

**Результат:** ingestion и retrieval по закреплённой revision с проверяемым provenance.

- [ ] **RUST-065 (M)** Определить domain model для KnowledgeDocument, Chunk, Revision и pinned revision.
  - Тесты: unit state transition matrix; serialization tests; invalid revision references.
  - DoD: staged/live/retired и revision identity типизированы.

- [ ] **RUST-066 (L)** Реализовать `commit_knowledge_revision` repository operation.
  - Тесты: DB integration — успешный commit, rollback, concurrent commit, revision log, event/outbox atomicity.
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
9. **PR-09:** RUST-044…050 — API walking skeleton, end-to-end и M1a acceptance.
10. Затем M1b → M2 → M3 → M4.

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