# Decorator Errors & The Transport Tunnel

> Spoke of `docs/ERRORS.md`. Read the switchboard first for the 4-tier
> overview and translation flow.

How decorator-injected errors flow from horizontal crates → through core
and the state layer (untouched) → to the product, where they're opened
and downcast to known types.

---

## Tier 0.5: Decorator Errors (Horizontal Crates)

Created by `ReadAt` decorators from horizontal crates (e.g., `archive-auth`,
`archive-retry`). Each horizontal crate defines its **own error type** —
there is no single shared enum. Each crate is independent.

```rust
// Owned by archive-auth crate.
#[non_exhaustive]
enum AuthError {
    TokenExpired,
    RefreshFailed { detail: String },
}

// Owned by server crate (not shared).
#[non_exhaustive]
enum QuotaError {
    Hit { used: u64, limit: u64 },
}

// Owned by archive-retry crate.
#[non_exhaustive]
enum RetryError {
    Exhausted { attempts: u32 },
}
```

Each error type implements `std::error::Error + Send + Sync`. Decorators
box them into `ReadError::Transport`. The box travels through core (as
`CoreError::Io(ReadError::Transport(box))`) and the state layer (as
`ArchiveError::Transport(box)`) untouched. The product at the top opens
the box and downcasts to the types it knows from its own dependencies.

---

## Decorator Construction (Product Assembles the Stack)

The **product** constructs the decorator stack using horizontal crates,
then passes the composed `ReadAt` to the state layer. The state layer
passes it to core. Neither the state layer nor core knows about decorators.

```rust
// Server product builds the stack:

fn open_cloud_archive(&self, url: &str) -> Result<Archive, ServerError> {
    // Bottom: core's dumb HTTP reader (streams response, returns ReadError)
    let http = HttpReadAt::new(url);

    // Decorate: product-level concerns from horizontal crates
    let authed  = AuthReadAt::new(http, self.token_provider.clone());   // archive-auth
    let retried = RetryReadAt::new(authed, self.retry_policy.clone());  // archive-retry
    let metered = MeteredReadAt::new(retried, self.quota_tracker.clone()); // server-internal

    // Pass composed ReadAt to state layer — returns Archive (the product-facing handle)
    let archive = self.state.open(metered).await?;  // returns Result<Archive, ArchiveError>

    Ok(archive)
}
```

```text
What core sees:          impl ReadAt ──── "give me bytes / give me a stream"
What's actually there:   MeteredReadAt → RetryReadAt → AuthReadAt → HttpReadAt
                         (server)        (retry crate)  (auth crate)  (core)

Error path:  HttpReadAt returns ReadError::Failed(kind: HttpStatus{401})
             AuthReadAt intercepts → refreshes token → retries
             If refresh fails → ReadError::Transport(AuthError::RefreshFailed)
             RetryReadAt sees Transport → not retryable, passes through
             MeteredReadAt sees Transport → not its concern, passes through
             Parser wraps as CoreError::Io(ReadError::Transport(box))
             State layer → ArchiveError::Transport(box)
             Server opens box → downcast to AuthError → ServerError::AuthFailed
```

A KMP app's stack might look completely different:

```rust
// KMP app — OAuth handled in Kotlin, passed as static header
fn open_cloud_archive(&self, url: &str, bearer_token: &str) -> Result<Archive, AppError> {
    let http = HttpReadAt::new(url)
        .with_headers(vec![("Authorization", format!("Bearer {bearer_token}"))]);

    // No auth decorator, no metering — just hand to state layer
    let archive = self.state.open(http).await?;
    Ok(archive)
}
```

---

## Product Layer: Open the Tunnel, Handle What You Know

The product assembled the decorator stack. It knows what error types are
possible because it depends on those horizontal crates. It opens
`ArchiveError::Transport`, tries to downcast, handles or reports.

```rust
// Pseudocode — product-level error handling (e.g., server).

fn handle_state_result<T>(&self, result: Result<T, ArchiveError>) -> Result<T, ServerError> {
    match result {
        Ok(val) => Ok(val),

        // --- OPEN THE TUNNEL: downcast to types I know about ---

        Err(ArchiveError::Transport(err_box)) => {
            // I composed AuthReadAt, so I know AuthError exists
            if let Some(auth) = err_box.downcast_ref::<AuthError>() {
                match auth {
                    AuthError::TokenExpired => {
                        // HANDLE: refresh token, retry
                        // if works: Ok(val) — error disappears
                        // if fails: Err(ServerError::AuthenticationFailed)
                    }
                    AuthError::RefreshFailed { .. } =>
                        Err(ServerError::AuthenticationFailed),
                }
            }
            // I composed MeteredReadAt, so I know QuotaError exists
            else if let Some(quota) = err_box.downcast_ref::<QuotaError>() {
                match quota {
                    QuotaError::Hit { used, limit } =>
                        Err(ServerError::QuotaExceeded { used: *used, limit: *limit }),
                }
            }
            // Unknown — nothing I composed produced this. Log and abort.
            else {
                Err(ServerError::Internal {
                    detail: format!("unexpected transport error: {err_box}")
                })
            }
        }

        // --- STATE ERRORS: translate or forward ---

        Err(ArchiveError::FileCorrupted { filename }) =>
            Err(ServerError::FileCorrupted { filename }),
        Err(ArchiveError::PasswordNeeded) =>
            Err(ServerError::PasswordNeeded),
        // ... remaining variants ...
        Err(other) =>
            Err(ServerError::Internal { detail: other.to_string() }),
    }
}
```

**Key insight:** A CLI that doesn't compose any decorators will never
receive `ArchiveError::Transport`. It can map that variant straight to
`Internal` — it's structurally unreachable. The tunnel only carries what
you put into it.
