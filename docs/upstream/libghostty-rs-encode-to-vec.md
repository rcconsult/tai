# Issue draft: libghostty-rs `encode_to_vec` can drop an event

Not filed. This is the issue tai would post to
[Uzaaft/libghostty-rs](https://github.com/Uzaaft/libghostty-rs), kept here
for reference. `docs/lessons/libghostty-vt-pitfalls.md` ("encode_to_vec
can drop a key or mouse event") describes how tai works around it. Before
filing, check that it's still present upstream and that nobody has
reported it already. Update the pin and line references if they've moved.

tai's test `a_key_queued_after_other_bytes_isnt_lost` (`src/main.rs`)
fails once the crate is fixed. Drop `ENCODE_ROOM` and tai-gui's
`encode_key` reserve then.

---

## encode_to_vec can fail with OutOfSpace when the Vec already holds bytes

**Crate:** libghostty-vt, from 0.2.2 to at least 575ed6b8ce on main.
**Files:** `crates/libghostty-vt/src/key.rs` (`Encoder::encode_to_vec`)
and `crates/libghostty-vt/src/mouse.rs` (the same pattern).

### What happens

`encode_to_vec` retries a too-small buffer like this:

```rust
let remaining = vec.capacity() - vec.len();
...
Err(Error::OutOfSpace { required }) => {
    // Retry with more capacity
    vec.reserve(required - remaining);
    self.encode_to_uninit_buf(event, vec.spare_capacity_mut())
}
```

`Vec::reserve(n)` guarantees capacity for `len + n` elements. Here `n` is
`required - remaining`, so after the call the spare capacity is only
guaranteed to be `required - remaining`, not `required`. The retry then
fails with the same `OutOfSpace`, and the event is lost. Amortised growth
often hides this, but not always.

### Reproduction

Seven bytes queued in an 8-byte Vec, then a key whose sequence is longer
than ten bytes (every Kitty keyboard flag on):

```rust
use libghostty_vt::{Terminal, key::{self, Key}};

let mut terminal = Terminal::new(80, 24).unwrap();
terminal.vt_write(b"\x1b[>31u");
let mut encoder = key::Encoder::new().unwrap();
let mut event = key::Event::new().unwrap();
event
    .set_action(key::Action::Press)
    .set_key(Key::A)
    .set_mods(key::Mods::CTRL | key::Mods::ALT | key::Mods::SHIFT | key::Mods::SUPER)
    .set_consumed_mods(key::Mods::empty())
    .set_unshifted_codepoint('a')
    .set_utf8(Some("A"));
encoder.set_options_from_terminal(&terminal);

let mut out = Vec::with_capacity(8);
out.extend_from_slice(b"1234567");
// Fails: Err(OutOfSpace { required: .. })
assert!(encoder.encode_to_vec(&event, &mut out).is_err());
```

In an application this showed up as a dropped keystroke, logged as "out
of space, 16 bytes required", when typed text and a special key landed in
the same output buffer within one frame.

### Suggested fix

Reserve the full requirement, since `reserve` already counts from the
length:

```rust
Err(Error::OutOfSpace { required }) => {
    // Retry with more capacity
    vec.reserve(required);
    self.encode_to_uninit_buf(event, vec.spare_capacity_mut())
}
```

`remaining` is then unused and can go. The same change applies to the
mouse encoder's `encode_to_vec`.

### Workaround

Reserve enough room before the call, for example `vec.reserve(64)`.
