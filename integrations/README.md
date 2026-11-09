# Host integrations

Keep framework/backend adapters here. They consume the Rust facade or a native language binding and own authentication/business authorization outside the guest. Create a fresh sandbox per task by default, grant explicit business scopes, pass an absolute request deadline, cancel on client disconnect and await explicit shutdown during host teardown. A timeout does not roll back an already committed database/network side effect. Do not expose a raw connection, query executor or secret to guest modules.
