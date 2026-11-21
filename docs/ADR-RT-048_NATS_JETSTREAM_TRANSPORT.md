# ADR RT-048 — NATS JetStream как транспорт QEmbed

- **Статус:** Accepted
- **Дата:** 2026-11-21
- **Область:** Rust Object Runtime, M1b и последующие распределённые компоненты
- **Связанные решения:** RT-039…RT-047; RUST-051…RUST-064

## Контекст

M1a сохраняет Invocation, events и Outbox в PostgreSQL одной транзакцией. Production Outbox Relay и durable worker lifecycle ещё не реализованы. M1b должен обеспечить асинхронную доставку заданий нескольким независимо работающим Worker instances.

Самостоятельная реализация полноценной очереди на PostgreSQL потребовала бы поддерживать broker-подобные механизмы: конкурирующие consumers, delivery leases, повторные доставки, backoff, backpressure, durable subscriptions и dead-letter handling. PostgreSQL остаётся необходимым для атомарной фиксации бизнес-состояния и намерения выполнить работу, но не обязан становиться транспортным брокером.

## Решение

**NATS JetStream принимается как штатный транспорт асинхронных команд и событий QEmbed. PostgreSQL остаётся единственным источником истины бизнес-состояния и единственным источником восстановления доставки.**

Это решение не означает, что JetStream хранит authoritative Invocation state, budgets, event history, tool outcomes или результаты выполнения. Broker state — производная, восстанавливаемая проекция PostgreSQL Outbox.

### 1. Границы ответственности

| Компонент | Владеет | Не владеет |
|---|---|---|
| PostgreSQL | Invocation/ToolExecution state, budgets, events, Outbox payload и delivery identity, результаты, leases/fencing и recovery facts | Broker subscriptions и распределением сообщений между consumers |
| Transactional Outbox | Атомарным сохранением намерения опубликовать сообщение вместе с бизнес-транзакцией | Фактом успешного выполнения задания |
| Outbox Relay | Чтением committed Outbox, публикацией в JetStream и фиксацией PubAck receipt | Бизнес-решением о retry/Unknown и результатом команды |
| NATS JetStream | Durable stream storage, delivery, consumer coordination, redelivery и transport-level flow control | Авторитетным состоянием QEmbed и окончательным исходом бизнес-операции |
| Worker | Выполнением допустимой работы, проверкой durable state, фиксацией результата и ACK после commit | Самостоятельным изменением tenant identity, budget policy или права на повтор side effect |
| Recovery Sweeper | Сопоставлением PostgreSQL runnable state с ожидаемым продвижением и повторной публикацией | Обходом Invocation/ToolExecution state machine или reconciliation |

### 2. Путь сообщения

1. API аутентифицирует запрос и в одной PostgreSQL-транзакции создаёт/обновляет Invocation, event и Outbox delivery. Commit завершается до любого NATS I/O.
2. Relay короткой PostgreSQL-транзакцией получает delivery claim (FOR UPDATE SKIP LOCKED, lease owner/expiry, monotonic fencing token). Транзакция завершается до сетевого вызова.
3. Relay публикует минимальный versioned envelope в JetStream. Стабильный delivery_id передаётся как Nats-Msg-Id. Relay ждёт JetStream PubAck.
4. Только после PubAck Relay сохраняет publication receipt в PostgreSQL. Если процесс упал между PubAck и записью receipt, сообщение будет опубликовано повторно.
5. Durable pull consumer получает сообщение. Worker проверяет durable Invocation/ToolExecution state и получает право на работу через существующие PostgreSQL leases/CAS/fencing.
6. Worker сохраняет результат и переход состояния в PostgreSQL. Только после успешного commit отправляет explicit ACK в JetStream.

Все внешние сетевые вызовы выполняются вне SQL-транзакций.

### 3. Гарантии доставки и идемпотентность

- Семантика — **at-least-once**, не exactly-once.
- JetStream message deduplication по Nats-Msg-Id ограничена настроенным временным окном. Она снижает дубли, но не является долговременной гарантией.
- Worker обязан быть идемпотентным по стабильной logical identity: как минимум delivery_id для transport delivery и invocation_id/command_id для бизнес-операции.
- Повторная доставка после DB commit, но до ACK не должна повторять уже завершённую бизнес-операцию.
- PubAck означает, что JetStream принял публикацию; он не означает, что Worker получил сообщение или бизнес-операция завершилась.
- Broker ACK отправляется только после commit результата/состояния в PostgreSQL.

### 4. PostgreSQL recovery — обязательная гарантия

Outbox не удаляется и не считается бизнес-завершённым только из-за PubAck. Publication receipt означает лишь «broker подтвердил приём этой публикации».

Recovery Sweeper периодически ищет runnable/queued Invocation, для которых нет активного execution lease и которые не продвигаются в пределах установленного progress deadline. Он выставляет durable replay-request marker в PostgreSQL для исходной Outbox row; Relay снова делает её claimable даже если предыдущая публикация уже получила PubAck. Marker не удаляет и не перезаписывает историю publication receipt. После повторной публикации Relay фиксирует новый receipt и закрывает replay request. Payload и delivery_id не меняются; используется та же logical delivery identity. Физическая схема replay marker и учёт publication attempts реализуются в RUST-052c/RUST-051b после сверки с canonical qembed-sql schema.

Повтор может создать дубликат, если исходное сообщение всё ещё находится в JetStream или уже обрабатывается. Это допустимо: durable Invocation state, idempotency keys, leases и fencing не дают повторно применить уже завершённый side effect.

Для Unknown и неоднозначного provider outcome Sweeper **не** запускает выполнение повторно. Сначала требуется reconciliation либо доказанная upstream idempotency в соответствии с существующим Runtime contract.

Relay claimable condition включает как первоначальную непубликованную запись, так и запись с активным durable replay request. Replay marker хранится в PostgreSQL и не зависит от NATS. Retention/cleanup Outbox определяется отдельной политикой, которая сохраняет payload и identity на весь период, необходимый для recovery и аудита. Нельзя удалять единственную копию задания после PubAck.

### 5. Message contract и trust boundary

- Использовать versioned subjects/envelopes; изменения payload требуют совместимого versioning.
- Передавать минимальный envelope с delivery_id, типом/версией сообщения и opaque references на durable business records.
- Не передавать секреты, raw provider credentials, чувствительные tool payloads или данные, которые Worker должен повторно получать из защищённого Store.
- Поля tenant/caller в сообщении не являются доказательством полномочий. Worker восстанавливает tenant и trusted execution context из PostgreSQL под service identity; сообщение — только сигнал о наличии работы.
- Broker credentials разделяются по process role и минимальным permissions. API не получает consumer permissions без необходимости; relay получает publish permissions; Worker получает только необходимые consumer permissions.

### 6. Retry, poison messages и DLQ

PostgreSQL остаётся авторитетным для бизнес-состояния, количества/причины бизнес-попыток, terminal/Unknown и recovery. JetStream redelivery/backoff и MaxDeliver используются как транспортный механизм и предохранитель, а не как замена durable business retry state.

Malformed/permanent-invalid envelope фиксируется как durable failure/parking outcome; transient transport/worker failure может быть redelivered. DLQ/parking stream — операционный инструмент диагностики, не источник истины и не единственное место хранения сообщения.

### 7. Rust integration и deployment boundary

- Domain/Runtime зависят от broker-neutral ports, например MessagePublisher и MessageConsumer, а не от async-nats types.
- async-nats используется как concrete Rust adapter только после dependency/MSRV audit. Текущий Rust 1.85 MSRV сохраняется, если отдельное согласованное решение не докажет необходимость повышения.

**Предварительный MSRV finding (2026-11-21):** опубликованный manifest async-nats 0.49.1 задаёт rust-version 1.88.0; release notes async-nats 0.51.0 отдельно сообщают о повышении MSRV до 1.89.0. Поэтому нельзя просто добавить latest async-nats в workspace с Rust 1.85. В RUST-051b необходимо проверить более раннюю версию, которая одновременно поддерживает нужный JetStream API и реально собирается на Rust 1.85, включая транзитивные зависимости и feature set. Если такой поддерживаемой версии нет, повышение workspace MSRV оформляется отдельным решением и не выполняется молча. Источники: [async-nats 0.49.1 Cargo manifest](https://docs.rs/crate/async-nats/0.49.1/source/Cargo.toml.orig) и [async-nats 0.51.0 release notes](https://github.com/nats-io/nats.rs/releases/tag/async-nats%2Fv0.51.0).
- Не добавлять седьмой crate. На первом этапе API, Relay и Worker могут быть разными process modes одного Rust workspace/artifact и запускаться/масштабироваться независимо.
- Выделение отдельных deployable services и дальнейшее дробление bounded contexts выполняются постепенно; NATS сам по себе не требует немедленного разбиения всех crates на микросервисы.
- Локальная разработка и CI должны запускать настоящий JetStream server для integration/failure tests; unit tests могут использовать test doubles только для чистых port contracts.

## Рассмотренные альтернативы

### Самописная PostgreSQL queue

Отклонена как основной transport. SQL claim/lease/fencing остаются полезными внутри Outbox Relay, но не будут расширяться до самостоятельного broker: consumer lifecycle, delivery coordination, retry/backoff и transport flow control делегируются JetStream.

### NATS Core pub/sub без JetStream

Отклонён для durable workload: ephemeral Core NATS delivery не предоставляет нужную нам durable stream/consumer модель.

### JetStream как второй источник истины

Отклонён. Broker data может быть потеряна, удалена по retention или недоступна; PostgreSQL обязан позволять восстановить публикацию и продолжить корректную обработку.

## Последствия

**Положительные:**
- меньше собственного broker-like кода;
- durable consumers и горизонтальное масштабирование Worker;
- отделение producer/consumer lifecycle;
- transport можно заменить за broker-neutral port без переписывания бизнес-логики;
- PostgreSQL recovery сохраняет контроль над бизнес-состоянием.

**Стоимость и ограничения:**
- дополнительный инфраструктурный компонент, его storage, monitoring, credentials, upgrades и HA;
- at-least-once требует обязательной идемпотентности и failure-injection tests;
- PostgreSQL recovery sweeper остаётся обязательным и не может быть заменён NATS;
- JetStream deduplication имеет конечное окно и не является exactly-once.

## Acceptance criteria

- [ ] PostgreSQL commit предшествует публикации в JetStream.
- [ ] Relay ждёт PubAck и сохраняет publication receipt в PostgreSQL.
- [ ] Crash после PubAck до DB receipt приводит к безопасной повторной публикации.
- [ ] Worker ACK происходит только после DB commit.
- [ ] Duplicate delivery не создаёт повторного бизнес-side-effect.
- [ ] NATS outage не теряет committed Outbox messages.
- [ ] Simulated broker message loss восстанавливается из PostgreSQL Outbox.
- [ ] Recovery не переисполняет terminal или Unknown operations без reconciliation.
- [ ] JetStream credentials и subjects разделены по process role.
- [ ] Integration/failure suite запускается против настоящего JetStream server.
