# Linux system service

The default Linux installation is a systemd **user** unit that keeps the master
key in the Secret Service. That requires a session bus, an unlocked keyring, and
`loginctl enable-linger` to survive an SSH logout — none of which a headless
server has by default.

`hsind service install --system` installs a **system** unit instead. A system
unit has no session bus, so the master key comes from systemd credentials.

## Runtime requirements

The binaries link nothing but glibc: SQLite is bundled, TLS is rustls with
built-in roots, and the Secret Service client is pure Rust. A system-scope
installation additionally needs:

- systemd 250 or newer, for `LoadCredentialEncrypted=` and `systemd-creds`
- write access to `/etc/systemd/system` and `/etc/hsin` (that is, root)

No D-Bus, no gnome-keyring, no `ca-certificates`.

## Install

```bash
sudo ./hsind service install --system --account "$USER" --start
```

What it does:

1. Copies `hsind` and `hsin` into the account's data home (`~/.local/share/hsin/bin`)
   and chowns the tree to that account.
2. Links both binaries into `/usr/local/bin`, because the data home is on
   nobody's PATH. An existing real file there is left alone rather than
   overwritten, and `uninstall` only removes links that point back at this
   installation.
3. Generates a master key, seals it with `systemd-creds encrypt` into
   `/etc/hsin/<home-scope>/hsin-master-key-v1.cred` (0600, bound to this host
   and to the TPM when one is present), and skips this step if a sealed key is
   already there.
4. Writes `/etc/systemd/system/hsind-<home-scope>.service` with `User=`,
   `Group=`, `HSIN_HOME=`, and `LoadCredentialEncrypted=`.
5. Runs `systemctl daemon-reload` and `systemctl enable --now`.

`--account` defaults to `$SUDO_USER`. The daemon and the CLI both run as that
account, so the IPC socket keeps its 0600 permissions and the managed Codex and
Claude configuration files keep their existing ownership.

At first start the daemon finds an empty database and a provisioned key store,
adopts the sealed key, and writes the matching key record. Confirm with:

```bash
systemctl status hsind-*.service
hsin doctor
```

## Moving an existing installation to system scope

The sealed credential replaces the Secret Service entry, and a system unit
cannot read the old one. Export the recovery key first, while the per-user
daemon still runs:

```bash
hsin security export-recovery-key          # keep this somewhere safe
hsind service uninstall                    # removes the user unit
sudo ./hsind service install --system --account "$USER" --recovery-key-stdin --start
```

`--recovery-key-stdin` reads one line from stdin, so the key never reaches
`argv` or the shell history. Provisioning refuses to overwrite a credential that
is already sealed; remove `/etc/hsin/<home-scope>` first if you really mean to
replace it.

## Limits

- **`uninstall` does not release managed client configuration.** Codex and
  Claude keep the base URL and the credential-helper command that pointed at the
  removed daemon, and `auth.json` keeps the `HSIN_MANAGED_KEY` placeholder that
  only a local proxy accepts. Switch the client back to an unmanaged provider
  *before* uninstalling, or restore its config by hand afterwards.
- **A taken proxy port fails the mode switch.** `hsin mode set <client> proxy`
  reports the bind error and leaves the mode unchanged. Point the listener
  somewhere free first:
  `hsin settings set --proxy-host 0.0.0.0 --proxy-port 9998 --proxy-enabled true`.
- **`hsin daemon update` and `hsin daemon install` do not work in system scope.**
  They reinstall in user scope, which would put a second unit on the same
  database, so they are rejected with a scope-mismatch error. Update by
  re-running `sudo ./hsind service install --system --account <name>` with the
  new binaries; the sealed credential is kept.
- **Key rotation is unavailable in system scope.** `$CREDENTIALS_DIRECTORY` is a
  read-only tmpfs, so the daemon cannot write a new key version; rotation fails
  before touching the database, with a message pointing back at
  `install --system`. To rotate, export the recovery key, remove
  `/etc/hsin/<home-scope>`, and re-provision.
- Sealing is host-bound. Restoring `/etc/hsin` onto a different machine will not
  decrypt; recover with the exported recovery key instead.
- `hsind service start|stop|restart|status --system --account <name>` must run as
  root, because they drive `systemctl` without `--user`.

## Uninstall

```bash
sudo ./hsind service uninstall --system --account "$USER"
sudo ./hsind service uninstall --system --account "$USER" --purge   # also removes the data home and the sealed key
```
