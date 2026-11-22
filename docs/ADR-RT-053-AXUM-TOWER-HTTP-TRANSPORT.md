# ADR-RT-053 — Axum + Tower как HTTP transport stack

- **Статус:** Accepted
- **Дата:** 2026-11-23
- **Область:** Rust Object Runtime, crate `qembed-api`
- **Связанные задачи:** RUST-044-T, RUST-044
- **Реализация:** PR [#62](https://github.com/qembed/qembed-object-runtime/pull/62), merge commit `2edc8bd2f5d3cedbc9d464211e5c4bbe883d7bd5`

## Контекст

Owner API и health/readiness endpoints требуют HTTP transport. До RUST-044-T HTTP слой был реализован на Actix Web. Перед завершением Owner API было принято заменить HTTP framework, чтобы закрепить единый transport stack и не расширять API поверх старого framework.

Миграция уже выполнена и находится в `main`. Текущий composition root возвращает `axum::Router`; workspace dependency manifest закрепляет Axum `0.8.4` и Tower `0.5.2`. Это ADR фиксирует фактическое архитектурное решение, а не инициирует повторную миграцию.

## Решение

**Axum + Tower — единственный поддерживаемый HTTP transport stack Rust runtime.**

- Axum отвечает за HTTP routing, extractors, handlers и композицию Router.
- Tower предоставляет общую абстракцию Service и middleware/layer stack.
- `qembed-api` владеет API Router, middleware, health/readiness routes и concrete composition root.
- Listener binding, process startup/shutdown и graceful shutdown принадлежат server executable/host, а не библиотечному API crate.
- Owner API сохраняет HTTP/JSON контракт. Миграция framework не меняет paths, methods, headers, status codes, DTOs, error mapping, response minimization или authentication contract.
- Owner authentication остаётся на существующей границе: Bearer token и недоверенный `X-Tenant-ID` selector; Principal/Scope создаются только после проверки persisted credentials.
- Для Owner routes сохраняется явный лимит тела запроса 256 KiB.
- PostgreSQL API integration tests используют Tower `oneshot` service execution, но продолжают обращаться к настоящему PostgreSQL. Router mocks не заменяют DB/security integration coverage.

## Границы зависимостей

- `qembed-domain` и `qembed-runtime` не зависят от Axum, Tower или конкретного HTTP framework.
- HTTP handlers выполняют transport parsing/validation, authentication boundary и response mapping; бизнес-use cases принадлежат Runtime, SQL — Store.
- SQLx и concrete Store adapters не импортируются HTTP handler modules.
- В workspace не должно быть зависимости `actix-web` или параллельного legacy HTTP server stack.
- Не добавлять отдельный crate только ради HTTP transport.

## Connect, gRPC и Protobuf

Это решение **не принимает Connect-Rust, gRPC или Protobuf как обязательный API transport** и не переводит Owner API с HTTP/JSON на RPC.

Если появится подтверждённая потребность в межсервисном RPC, streaming или generated Protobuf contracts, она рассматривается отдельным ADR. Такой RPC adapter должен вызывать те же Runtime use cases и не переносить transport-specific types в Domain/Runtime contracts. Решение о Connect/gRPC не является основанием менять текущий Owner HTTP contract.

## MSRV и зависимости

- На момент реализации RUST-044-T workspace сохранял Rust 1.85; Axum + Tower migration сама по себе не потребовала повышения MSRV.
- Текущая конфигурация workspace на дату этого ADR закрепляет Rust 1.100 в `Cargo.toml` и `rust-toolchain.toml`. Это последующее состояние toolchain и не является причиной или частью миграции RUST-044-T.
- Прямые версии workspace dependencies закреплены: Axum `=0.8.4`, Tower `=0.5.2`. Изменения этих версий проходят обычный dependency/MSRV audit и CI.

## Проверка и evidence

PR #62 выполнил transport-only migration:

- Axum Router/handlers заменили прежний HTTP routing и composition.
- Owner routes и health/readiness поведение сохранены.
- Добавлен/сохранён явный лимит тела Owner requests 256 KiB.
- API integration tests переведены на Tower `oneshot`, PostgreSQL coverage сохранён.
- Runtime, Domain, Store behavior, SQL schema и Owner HTTP contract не менялись.

CI workflow [#44728030212](https://github.com/qembed/qembed-object-runtime/actions/runs/44728030212) завершился успешно на точном PR HEAD `26bd7f142aa57cab2c994165a4825fb632bf40cc`, включая stable и Rust 1.85 MSRV checks, PostgreSQL integration tests, Clippy, dependency/architecture checks и SQLx offline metadata validation.

Текущий `main` использует Axum в `qembed-api`; workspace `Cargo.toml` содержит Axum/Tower и не содержит `actix-web`.

## Последствия

**Положительные:**

- единый Tower-compatible service/middleware model;
- Axum Router можно объединять с другими Tower-compatible services;
- Owner HTTP contract и существующие security boundaries не зависят от framework;
- transport framework остаётся за пределами Domain/Runtime.

**Ограничения:**

- Axum/Tower не дают автоматически Connect/gRPC contracts, Protobuf schema evolution или streaming RPC;
- process lifecycle и listener management остаются ответственностью будущего host executable;
- любые новые middleware должны сохранять tenant/auth isolation, body-size limits и безопасное error mapping.

## Acceptance

- [x] Axum используется для всех HTTP routes в `qembed-api`.
- [x] Tower используется как service/middleware abstraction и для router integration tests.
- [x] Owner HTTP/JSON contract и auth/tenant boundaries сохранены.
- [x] Health/readiness поведение сохранено.
- [x] PostgreSQL API integration tests продолжают использовать настоящую БД.
- [x] Workspace dependencies не содержат `actix-web`.
- [x] Stable и Rust 1.85 migration CI прошли на точном PR HEAD.
- [x] RUST-044-T закрыта; последующая toolchain policy отражена отдельно в Roadmap.
