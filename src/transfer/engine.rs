//! Transfer engine: expands jobs into items, schedules them with a
//! semaphore of `concurrency` slots, applies pause/cancel through
//! `tokio::sync::watch`, raises and resolves conflicts, retries, parks items
//! as `WaitingForReconnect` on lost connections, resumes `.scpto-part`
//! files whose `partials.json` record matches the source, and performs
//! moves (rename within an endpoint, copy-then-delete across endpoints).
