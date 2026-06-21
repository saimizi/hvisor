# Multiboot2 and How GRUB Loads hvisor (x86_64)

A focused explainer on what a "Multiboot2 kernel" is and exactly how GRUB loads
hvisor on the x86_64 / QEMU target. This complements
[`boot_x86_64_qemu.md`](boot_x86_64_qemu.md), which covers the whole boot chain;
here we zoom in on the GRUB ↔ hvisor handoff.

> TL;DR: hvisor is a **normal ELF** that also embeds a **Multiboot2 header** in
> its first bytes. GRUB finds that header by a raw byte scan (no ELF parsing
> needed) and — because hvisor supplies an *address tag* — loads it as a flat
> blob using explicit addresses, not via the ELF program headers.

---

## 1. What is a Multiboot2 kernel?

**Multiboot** is a specification (from the FSF; Multiboot2 is the modern
revision) that defines a contract between a bootloader (such as GRUB) and the
kernel it boots. A *Multiboot2 kernel* is simply a kernel that honors this
contract, so a compliant loader can boot it **without knowing anything
kernel-specific**.

The contract has two halves:

**a) The kernel embeds a Multiboot2 header** near the start of its image — a
magic-tagged structure the bootloader scans for. In hvisor this is the block in
[`src/arch/x86_64/multiboot.S`](../src/arch/x86_64/multiboot.S):

```asm
multiboot2_header:
    .int  0xe85250d6          // magic = "I am a Multiboot2 kernel"
    .int  0                   // architecture = i386
    .int  multiboot2_header_len
    .int  -(magic + arch + len)   // checksum: the four fields sum to 0
```

The header also carries **tags** stating what the kernel wants/needs, e.g.
`tag_address` (where to load), `tag_entry_address` (where to jump), and
`tag_framebuffer` (graphics mode).

**b) The kernel accepts the boot-time contract.** When the loader transfers
control, Multiboot2 guarantees a known CPU state:

- 32-bit **protected mode**, paging **off**, interrupts off
- `eax` = a magic value (`0x36d76289`) so the kernel can *verify* it was
  Multiboot2-booted
- `ebx` = pointer to the **Multiboot information structure** (the "boot info")

hvisor's `arch_entry` captures exactly this:

```asm
mov edi, eax    // magic — proof we were Multiboot2-booted
mov esi, ebx    // pointer to the boot info structure
```

### The boot info structure (≈ the DTB of x86)

The `ebx` pointer is the payload: a list of tags the loader fills in so the
kernel need not probe the machine itself. hvisor parses these in
`boot::multiboot_init` ([`src/arch/x86_64/boot.rs`](../src/arch/x86_64/boot.rs)):

- **Memory map** — which physical RAM exists.
- **Modules** — extra files GRUB loaded alongside the kernel. hvisor's
  `module2` lines in `grub.cfg` load zone0's Linux kernel + initramfs this way;
  hvisor finds them here and builds zone0 from them.
- **ACPI tables** — pointers to firmware tables.

This is the same role the **DTB** plays on aarch64.

---

## 2. Is hvisor an ELF or a Multiboot2 kernel? — Both

These are not mutually exclusive. hvisor *is* a fully valid ELF (with ELF
header, program headers, symbols, sections), and it *also* embeds a Multiboot2
header as **data inside** that ELF:

```
/boot/hvisor  =  a standard ELF file
┌─────────────────────────────────────────────┐
│ ELF header + program headers                  │  ← valid ELF
│ .text:                                        │
│   .text.header   ← Multiboot2 header (data)   │  ← valid Multiboot2 kernel
│   .text.entry    ← arch_entry (code)          │
│   .text.entry32 / .entry64 ...                │
│ .rodata / .data / .bss ...                    │
└─────────────────────────────────────────────┘
```

The linker forces the header to the front (so it lands in the first 32 KiB) —
[`platform/x86_64/qemu/linker.ld`](../platform/x86_64/qemu/linker.ld):

```ld
.text : {
    KEEP(*(.text.header))   // first in .text = start of image
    *(.text.entry)
    ...
}
```

`KEEP(...)` prevents the linker from garbage-collecting the header, since no code
"calls" it — it is data the bootloader reads.

---

## 3. How GRUB actually loads hvisor

### Step A — Find the header (raw scan, no ELF parsing)

`grub.cfg` says `multiboot2 /boot/hvisor`. GRUB opens that file and scans its
**first 32 KiB as raw bytes**, 8-byte aligned, looking for the magic
`0xe85250d6`. Finding the header does **not** require parsing ELF — that is why
the spec mandates "within the first 32 KiB, 8-byte aligned" and why the linker
puts `.text.header` first.

### Step B — Load the image (two possible paths)

Once the header is found, *how* GRUB loads depends on the header's tags:

- **Path A — address tag present → flat load (no ELF).** GRUB uses the explicit
  addresses from the tag: copy `[load_addr, load_end_addr)` to physical memory,
  zero BSS to `bss_end_addr`, jump to `entry_addr`. The ELF structure is ignored.
- **Path B — no address tag → GRUB's built-in ELF loader.** GRUB parses the ELF
  program headers to place segments.

**hvisor takes Path A**, because its MB2 header includes both tags:

```asm
tag_address:                          // type 2
    .int  multiboot_header - offset   // header_addr  (physical)
    .int  skernel         - offset    // load_addr
    .int  edata           - offset    // load_end_addr
    .int  bsp_boot_stack_top - offset // bss_end_addr
tag_entry_address:                    // type 3
    .int  arch_entry      - offset    // entry_addr
```

(`- offset` converts each high virtual link address, `offset =
0xffff_ff80_0000_0000`, to the physical address GRUB loads to.) So GRUB loads
hvisor as a flat blob and **never consults the ELF program headers**.

### Why keep it an ELF then?

GRUB does not strictly need the ELF for *this* config. The ELF format is kept
because it is the natural toolchain output and carries **symbols / debug info**
(useful for gdb, objdump, stack traces). On aarch64/riscv the build strips it to
a flat `.bin` via `objcopy`; on x86 the ISO simply ships the ELF and GRUB reads
the loadable bytes out of it via the address tag.

---

## 4. The file-offset formula (a.out kludge)

To know where in the file the load region begins, GRUB computes:

```
file_offset_to_load_from = (offset where magic was found) - (header_addr - load_addr)
```

The spec requires **`load_addr ≤ header_addr`**, so `(header_addr - load_addr)`
is always ≥ 0 (never negative).

For **hvisor** the two addresses coincide. In `linker.ld`, `skernel` and the
first byte of `.text` (the header) are the same location:

```
multiboot_header == skernel == BASE_ADDRESS
=>  header_addr == load_addr
=>  (header_addr - load_addr) == 0
=>  file_offset_to_load_from == offset where magic was found
```

Intuitively: the header *is* the first thing in the image, so "where the header
starts" and "where loading starts" are the same point. The non-zero case only
arises for kernels that place loadable content *before* their header
(`load_addr < header_addr`); hvisor does not.

---

## 5. Is the Multiboot2 header loaded into memory?

**Yes — loaded, but never executed.**

Since `load_addr == header_addr == skernel`, the loaded region
`[skernel, edata)` (`.text` → `.rodata` → `.data`) begins exactly at the header.
So the header bytes are copied into physical memory at `load_addr` (physically
`0x200000`) along with everything else.

But GRUB jumps to `entry_addr = arch_entry`, which sits *after* `.text.header` in
the layout. The CPU never executes the header — after load it is just inert data
parked at the start of the image. This is normal: the header must exist both on
disk (for GRUB to find) and in the loaded image, so it lives in a loadable
section and is simply ignored at runtime.

---

## Summary

1. A Multiboot2 kernel = a kernel that embeds a Multiboot2 header and accepts the
   Multiboot2 boot-state contract.
2. hvisor is **both** a normal ELF **and** a Multiboot2 kernel; the header is
   data placed first in `.text`.
3. GRUB **finds** the header by a raw 32 KiB byte scan — no ELF parsing.
4. GRUB **loads** hvisor via the **address tag** (flat copy), not the ELF
   program headers; the ELF survives mainly for tooling/symbols.
5. The load formula's `(header_addr - load_addr)` is `0` for hvisor, because the
   header sits at the very start of the image.
6. The header **is** loaded into memory but is **never executed** — `arch_entry`
   is the real entry point.
