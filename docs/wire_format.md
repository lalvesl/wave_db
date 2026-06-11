# WaveDB Wire Format (replaces serde + postcard)

Goal: shrink the WASM binary by deleting serde's generic machinery, and give the
engine a layout it can reason about statically — every `(struct_id, version)`
pair has a compile-time-known **stack size** and a heap section whose shape is
described by the struct's field descriptors.

## Layout

A serialised value is two contiguous sections:

```
[ STACK section — exactly T::STACK_SIZE bytes, known at compile time ]
[ HEAP section  — variable bytes, length = value.heap_size()          ]
```

- **Stack section**: every fixed-width field packed little-endian in
  declaration order, no padding. Every _dynamic_ field contributes a fixed
  slot to the stack section too (its `u32` heap-length, plus flag/tag bytes),
  so all stack offsets are compile-time constants.
- **Heap section**: the dynamic payloads, appended in depth-first field
  declaration order. A parser walks it sequentially using the `u32` length
  slots from the stack section.

Serialisation allocates **once**: `Vec::with_capacity(T::STACK_SIZE +
value.heap_size())`.

## Per-type encoding

| Type                              | Stack bytes                                          | Heap bytes                      |
| --------------------------------- | ---------------------------------------------------- | ------------------------------- |
| `u8..u128`, `i8..i128`, `f32/f64` | width, LE                                            | —                               |
| `bool`                            | 1 (`0`/`1`)                                          | —                               |
| `char`                            | 4 (`u32` scalar)                                     | —                               |
| `Id`                              | 16                                                   | —                               |
| `[T; N]`                          | `N * T::STACK_SIZE`                                  | elements' heap, in order        |
| `String`                          | `u32` byte-length                                    | UTF-8 bytes                     |
| `Vec<T>`                          | `u32` region byte-length                             | element units, back-to-back     |
| `Option<T>`                       | `1` flag + `T::STACK_SIZE` (zero-filled when `None`) | `T`'s heap when `Some`          |
| struct                            | sum of field stack sizes                             | fields' heap, declaration order |
| enum, all variants field-less     | 1 (tag)                                              | —                               |
| enum, any variant with fields     | 1 (tag) + `u32` payload length                       | variant fields as a unit        |
| tuple                             | sum of member stack sizes                            | members' heap, in order         |

`usize`/`isize` are **not** encodable — wire layout must not depend on the
platform.

## Composition rules

Two ways a value nests, and they are different on purpose:

1. **Flattened** (struct field inside a struct, tuple member, array element,
   the `T` inside `Option<T>`): the child's stack slots are emitted inline
   into the parent's stack section; the child's heap payloads are appended to
   the shared heap section in field order. This is what keeps every stack
   offset a compile-time constant.
2. **Unit** (each `Vec<T>` element, an enum's variant payload): the value is
   self-contained — `[child stack][child heap]` back-to-back inside the
   parent's heap region. Elements parse sequentially: read `T::STACK_SIZE`
   bytes, the child's own length slots say how much heap follows, repeat
   until the region is exhausted (the region length is the parent's `u32`
   slot, so no element count is stored).

## Record envelope and the registry header

A top-level record is prefixed with a `u32` header:

```
header = (STRUCT_ID as u24) << 8 | STRUCT_VERSION as u8
[ u32 header LE ][ stack ][ heap ]
```

All nodes (quick, slow, client/WASM) build a **static registry** at compile
time via the `declare_objects!` macro: one module per `struct_id`, every
version declared, searchable by the `u32` header. The registry exposes
`&'static ObjectDescriptor`s — stack size, shape, field table (name, stack
offset, kind, heapable flag), heap-field name list — so the storage engine can
locate any field of any declared object without deserialising, organise
anchors/indexes for `NonUnique`/`NestedNonUnique`, and dispatch statically
(match on header → monomorphised fn, no `dyn`).

## Trade-offs vs postcard

- No varints: integers cost their full width before compression. The
  per-STRUCT dictionary compressor eats the constant zero runs; predictable
  offsets are worth more than pre-compression byte count.
- `Option<T>` reserves `T`'s stack slots even when `None` (e.g.
  `Metadata.permission: Option<PermissionRef>` is a constant 6-byte slot, not
  postcard's 1 byte). Same rationale: fixed offsets, dictionary-friendly.
- In exchange: single-allocation writes, zero-copy-friendly sequential reads,
  compile-time sizes, no serde/postcard code in the binary.
