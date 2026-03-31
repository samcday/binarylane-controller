# Server Password Management Plan

## Status

Deferred for follow-up. We are intentionally not implementing password reset reconciliation in this change because password reset may restart or disrupt running servers.

## Current baseline (implemented now)

- Autoscaler scale-up generates a password per node.
- The same password is sent in BinaryLane `create_server` requests to suppress random-password emails.
- The password is upserted into a namespaced Secret named `<node-name>-node-password` with key `password`.
- Node finalizer removes this Secret before clearing the finalizer.

## Proposed follow-up behavior

1. **Trigger detection**
   - Reconcile each managed node against Secret `<node-name>-node-password`.
   - Consider a reset required when:
     - Secret is missing, or
     - Secret `resourceVersion` changed since last successful reset.

2. **Reset operation**
   - Call BinaryLane `password_reset` action with the Secret password.
   - Persist success by writing annotation `bl.samcday.com/reconciled-at` on the Secret.
   - Record `bl.samcday.com/reconciled-resource-version` to avoid repeated resets.

3. **Missing Secret strategy**
   - If Secret is deleted, generate a new password, recreate the Secret, and run password reset.

4. **Failure handling**
   - Never remove node finalizer based on reset outcomes.
   - Emit Kubernetes events with reason codes (e.g. `PasswordResetFailed`).
   - Back off retries to avoid action storms.

5. **Safety controls**
   - Add opt-in flag (e.g. `PASSWORD_RECONCILE_ENABLED=false` by default initially).
   - Document that password reset may bounce nodes depending on image/agent support.

## Open questions

- Which OS images support in-place password changes without restart reliably?
- Should we gate reset behavior by node annotation or by global config only?
- Do we need a cooldown window between consecutive password resets per server?
