# Run results go only to Sessions a hook attested

A finished Run's result, including the end of its Job output, is queued as a prompt in the Session that submitted it. Anyone who can choose that destination can put text in another conversation. Hook annotation already replaced a `session` the model wrote, but two paths still took the caller's word: an explicit `session` in `submit_run` when no hook ran, and `CODEX_THREAD_ID` in a `geleider run` shell. The daemon now keeps a destination only when the call carries a single-use nonce that a PreToolUse hook (or the OpenCode plugin, through `geleider hook`) sent it with the same Session, less than 10 minutes earlier, over the hook socket. Anything else is queued as a Run with no destination, and the reply says delivery is off.

The shell path is dropped, not attested. A Codex shell PreToolUse hook sees the command text, not the `geleider` process that the command eventually starts, so tying the two together would mean parsing shell. `geleider run --wait` and `geleider wait` cover shells. The documented fallback of passing your own Session id when hooks are off is dropped too: an id the model writes is the claim being defended against.

## Considered Options

- **Attestation over the hook socket**: chosen. It keeps the hook nonblocking, needs no secret on disk, and a nonce is useless once spent or expired.
- **A long-lived secret per Harness install** given to hooks and checked by the daemon: any process that can read the hook config or the hook's environment could read it too, and it never expires.
- **Checking the caller's process ancestry** against each Session's owner pid: desktop apps run many Sessions in one process, so it cannot tell them apart.
- **Keeping the explicit fallback, documented as unauthenticated**: the state before this decision.

## Consequences

The attestation proves that something able to write `hooks.sock` chose the destination. Every process of this user can do that, and could equally run `codex queue` itself, so the remaining boundary is the Harness sandbox: one that lets a model's shell reach geleider's state directory lets that model attest any Session.
