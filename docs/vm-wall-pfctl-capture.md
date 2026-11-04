# Capturing `pfctl` output as root (VM wall)

`src/wall/pfctl.rs` parses what `pfctl` prints. Tracking: rejifald/faber-legacy#206.

**Captured as root on 2026-10-21** (macOS 27.0, before setup, so without
faber's own anchor). What it confirmed:

- `-s rules` prints the anchor point as `anchor "com.apple/*" all`, the one form
  the checker accepts.
- `-v -s rules` prints each rule's `[ Evaluations: … ]` line, then an
  `[ Inserted: uid 0 pid … ]` line. The parser ignores the second one.
- `-v -s Anchors` lists nested anchors (`com.apple/200.AirDrop`, …), indented.
- `-v -s Interfaces` prints the group as **`ALL`**, not `all`. The parser
  required `all` and would have refused every real listing, so the wall would
  never have turned green (fail closed). It now matches the group name in any
  case; `root_output_from_macos_27_parses` holds output of that shape.

Faber's own anchor (`com.apple/000.faber-vm-wall`) and its table could only be
captured after setup; that capture follows.

**Captured as root on 2026-10-24** (macOS 27.0, after setup, with `bridge100`
bound). What it confirmed:

- `-s rules` still prints the anchor point as `anchor "com.apple/*" all`.
- `-v -s Anchors` recurses: faber's anchor and the internet-sharing children are
  listed, not only the top level.
- The counter lines are as before.
- `-v -s Interfaces` has no `(skip)` marker on any line: stock macOS 27 has no
  `set skip` in `pf.conf`. It lists the bound `bridge100`. It also still lists
  a stale bridge that no longer exists, because pf keeps the interfaces it has
  seen. `root_interface_listing_with_a_bridge_bound_parses` holds a listing of
  that shape (names only).
- faber's own verdict was `open`.

The verdict fails closed on the two cases this capture did not show: a bound
bridge that pf does not list is red (rejifald/faber-legacy#309), and so is one listed
with `(skip)`. Details are in rejifald/faber-legacy#206.

The unprivileged `-n` form prints the anchor point as `anchor "/*" all`. The
checker accepts only `anchor "com.apple/*" all` in `pfctl -sr`: anything else
is red, so a different root print-back fails closed (the wall never turns
green) rather than open. Widening that match is a fail-open decision, so it
waits for the real output.

## What the owner runs on the builder

One block, as an admin user, on a macOS host with VM networks bound. It only
reads pf state. The full capture stays in a local directory (`$out`): keep it
there, because it maps the host's network. The `faber_host` table and the NAT
and rule listings carry its addresses and subnets, the interface listing shows
its adapters, tunnels and bridges, and an anchor can name other software it
runs. To rejifald/faber-legacy#206, or any other issue, post only its shape: the
answers to *What to look for*, quoting a line only once every address, host
name and interface name other than a VM bridge's is removed or replaced with a
placeholder such as `192.0.2.1`.

Every `pfctl` call is written out because zsh does not word-split an unquoted
`$args`, so a loop that passes `sudo pfctl $args` hands pfctl `-v -s Interfaces`
as one argument and fails.

```sh
out=$(mktemp -d)
sudo pfctl -s info > "$out/info.txt" 2> "$out/info.err"
sudo pfctl -s rules > "$out/rules.txt" 2> "$out/rules.err"
sudo pfctl -v -s rules > "$out/rules-v.txt" 2> "$out/rules-v.err"
sudo pfctl -a com.apple -s Anchors > "$out/anchors-com-apple.txt" 2> "$out/anchors-com-apple.err"
sudo pfctl -v -s Anchors > "$out/anchors-all.txt" 2> "$out/anchors-all.err"
sudo pfctl -v -s Interfaces > "$out/interfaces.txt" 2> "$out/interfaces.err"
sudo pfctl -s nat > "$out/nat.txt" 2> "$out/nat.err"
sudo pfctl -a com.apple/000.faber-vm-wall -v -s rules > "$out/anchor-rules.txt" 2>&1
sudo pfctl -a com.apple/000.faber-vm-wall -t faber_host -T show > "$out/table.txt" 2>&1
for f in "$out"/*; do echo "=== $(basename "$f")"; cat "$f"; done
```

## What to look for

- `rules.txt`: is the anchor point `anchor "com.apple/*" all`, or another form?
- `anchors-all.txt`: does it list nested anchors (`com.apple/250.…`, and any
  deeper path), or only the top level? The checker now also asks each listed
  anchor for its children, so either is safe; this tells us if that is needed.
- `rules-v.txt`: the `[ Evaluations: … ]` counter lines and their indentation.
- `interfaces.txt`: stock macOS 27 shows no `(skip)` at all. Any `(skip)` on a
  VM bridge is red, and so is a bound bridge that is not listed.

Fixtures for both candidate anchor forms are in `src/wall/pfctl.rs`.
