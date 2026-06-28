# The hvisor Linker Script (x86_64 / QEMU)

A walkthrough of [`platform/x86_64/qemu/linker.ld`](../platform/x86_64/qemu/linker.ld):
how the hypervisor's static image is laid out, and how its boundary symbols feed
the runtime memory layout computed in [`src/consts.rs`](../src/consts.rs).

> Related: [`boot_x86_64_qemu.md`](boot_x86_64_qemu.md) (the boot chain that uses
> this layout) and [`multiboot2.md`](multiboot2.md) (why the Multiboot2 header
> must be first in `.text`).

---

## The header (lines 1–3)

```ld
ENTRY(arch_entry)
BASE_ADDRESS = 0xffffff8000200000;
CPU_NUM = 4;
```

- **`ENTRY(arch_entry)`** — the ELF entry symbol. For the QEMU/GRUB path the
  Multiboot2 `entry_address` tag also names `arch_entry`, so both agree.
- **`BASE_ADDRESS = 0xffffff8000200000`** — the **virtual** address the image is
  linked at. It decomposes as:

  ```
  0xffffff8000200000  =  0xffff_ff80_0000_0000  +  0x20_0000
                          └── PHYS_VIRT_OFFSET ──┘     └ 2 MiB ┘
  ```

  That offset is `X86_PHYS_VIRT_OFFSET` from `entry.rs`. hvisor runs at high-half
  virtual addresses, but its **physical** load address is `0x200000` (2 MiB).
  This is why `multiboot.S` writes every address as `symbol - {offset}`: to turn
  the linked virtual address into the physical one GRUB loads to.
- **`CPU_NUM = 4`** — used only by the `.percpu` block below.

### How BASE_ADDRESS is decomposed?

In `src/arch/x86_64/entry.rs`, there is the following definition:
```rust
const X86_PHYS_VIRT_OFFSET: usize = 0xffff_ff80_0000_0000;
```

Since hvisor uses a linear address mapping, this value defines the offset from
physical address to virtual address:

```
virtual address = X86_PHYS_VIRT_OFFSET + physical address
```

In other words, `X86_PHYS_VIRT_OFFSET` is the virtual address mapped to physical
address 0.

So the physical address of `BASE_ADDRESS` can be calculated as follows:

```
0xffff_ff80_0020_0000 - 0xffff_ff80_0000_0000 = 0x200000
```

This can also be verified by the Multiboot2 address tag (`tag_address`) in
(`src/arch/x86_64/multiboot.S`):
```asm
.align 8
.type tag_address STT_OBJECT
tag_address:
    .short  multiboot2_header_tag_address
    .short  0
    .int    24
    .int    multiboot2_header - {offset}        // header_addr
    .int    skernel - {offset}                  // load_addr
    .int    edata - {offset}                    // load_end_addr
    .int    bsp_boot_stack_top - {offset}       // bss_end_addr
```

where the `load_addr` is the physical address to which the bootloader (`grub`)
will load the hvisor binary.

The `offset` placeholder is substituted by the `global_asm!` macro in
`src/arch/x86_64/entry.rs` — it is a compile-time `const` operand, not a global
variable:

```rust
global_asm!(
    include_str!("multiboot.S"),
    multiboot_header_magic = const MULTIBOOT_HEADER_MAGIC,
    multiboot_header_flags = const MULTIBOOT_HEADER_FLAGS,
    multiboot2_header_magic = const MULTIBOOT2_HEADER_MAGIC,
    multiboot2_arch_i386 = const MULTIBOOT2_ARCH_I386,
    rust_entry = sym rust_entry,
    rust_entry_secondary = sym rust_entry_secondary,
    offset = const X86_PHYS_VIRT_OFFSET,
    per_cpu_size = const PER_CPU_SIZE,
    cr0 = const CR0,
    cr4 = const CR4,
    efer_msr = const IA32_EFER,
    efer = const EFER,
);
```
So `{offset}` expands to `X86_PHYS_VIRT_OFFSET`, and `skernel` is `BASE_ADDRESS`
(the start virtual address of the `hvisor` binary) — confirming
`load_addr = BASE_ADDRESS - offset = 0x200000`, the physical address.

---

## The location counter `.`

Everything inside `SECTIONS` is driven by the special variable `.` (the current
output address):

- `. = BASE_ADDRESS;` sets the start.
- each output section advances `.` by its size.
- `. = ALIGN(4K);` rounds `.` up to a 4 KiB boundary.
- assignments like `skernel = .;` capture the current value into a symbol. These
  symbols are imported into Rust/asm as `extern "C"` (see `consts.rs`) so the code
  can refer to image boundaries.

---

## `.text` (lines 10–17)

```ld
stext = .;
.text : {
    KEEP(*(.text.header))   // Multiboot2 header — forced to the very front
    *(.text.entry)          // arch_entry (32-bit stub)
    *(.text.entry32)        // bsp_entry32 / ap_entry32
    *(.text.entry64)        // bsp_entry64 / ap_entry64
    *(.text .text.*)        // everything else
}
```

The ordering is deliberate and central to booting:

- **`.text.header` first** — guarantees the Multiboot2 header lands in the first
  32 KiB, where GRUB scans for it. `KEEP(...)` stops `--gc-sections` from dropping
  it, since no code references the header (it is data the bootloader reads).
- then the **entry stubs** in boot order, so the early 32-bit → 64-bit code is
  contiguous right after the header.
- then all remaining code.

---

## `.rodata`, `.data`, `.bss` (lines 19–43)

```ld
. = ALIGN(4K); etext = .; srodata = .;
.rodata : { *(.rodata .rodata.*) *(.srodata .srodata.*) }

. = ALIGN(4K); erodata = .; sdata = .;
.data : {
    *(.data.entry_page_table)   // placed first, on purpose
    *(.data .data.*)
    *(.sdata .sdata.*)
}

. = ALIGN(4K); edata = .;
.bss : {
    *(.bss.stack)               // boot stack, first in BSS
    sbss = .;
    *(.bss .bss.*)
    *(.sbss .sbss.*)
}
```

Standard layout, with two notable "first" entries:

- **`.data.entry_page_table` first in `.data`** — the early boot page-table data
  is grouped at a known spot. Sections are 4 KiB-aligned (`ALIGN(4K)`) because
  page tables and per-section permissions need page granularity.
- **`.bss.stack` first in `.bss`**, and `sbss` is captured *after* it. So the
  boot stack lives in `.bss` but sits *before* the zeroed range: `clear_bss()`
  zeroes only `sbss .. ebss`. The stack is therefore deliberately excluded from
  BSS zeroing.

---

## The `.percpu` block (lines 45–54) — the tricky part

```ld
. = ALIGN(4K);
_percpu_start = .;
_percpu_end   = _percpu_start + SIZEOF(.percpu);
.percpu 0x0 (NOLOAD) : AT(_percpu_start) {
    _percpu_load_start = .;
    *(.percpu .percpu.*)
    _percpu_load_end = .;
    . = _percpu_load_start + ALIGN(64) * CPU_NUM;
}
. = _percpu_end;
```

This implements CPU-local variables for the `percpu` crate. The mechanism:

- **`.percpu 0x0 ...`** — the section's **VMA (virtual address) is forced to
  `0x0`**, so every per-CPU variable is linked as an *offset from zero*, not a
  real address. At runtime, accessing per-CPU var `X` on the current core is
  `base_register + &X`, where the base register (on x86, the `gs` base) points at
  *this* CPU's copy. Linking at 0 is what makes `&X` a pure offset.
- **`(NOLOAD)`** — occupies address space but is not loaded from the file; the
  template is initialized/copied at runtime by `percpu::init()`.
- **`AT(_percpu_start)`** — sets the **LMA (load address)** to the real location
  in the image, even though the VMA is `0x0`. VMA and LMA diverge here:
  addressed-as-0, stored-at-`_percpu_start`.
- **`. = _percpu_load_start + ALIGN(64) * CPU_NUM;`** — reserves room for
  **CPU_NUM copies**: one CPU's block (its variables, rounded up to 64 bytes) ×
  4 CPUs.
- **`. = _percpu_end;`** — because the block ran the counter from `0x0`, this
  restores `.` to the real layout position so the rest of the script continues
  correctly.

> Do not confuse this `.percpu` section with the `PER_CPU_SIZE` (512 KiB) areas
> below. They are two different per-CPU concepts: `.percpu` holds small CPU-local
> globals accessed via the `gs` base; the 512 KiB areas hold each CPU's stack +
> `PerCpu` struct.

---

## End markers and what lives *after* the image (lines 56–67)

```ld
. = ALIGN(4K); ebss = .; ekernel = .;
/DISCARD/ : { *(.eh_frame) }      // drop unwind tables (no_std, no unwinding)
. = ALIGN(4K); __core_end = .;
}
__hv_end = __core_end + HV_EXTENDED_SIZE;
```

`__core_end` marks the end of the **statically linked image**. Everything past it
is *not* in the ELF — it is runtime-reserved RAM, computed in `consts.rs`:

```
__core_end ──► [ per-CPU areas: MAX_CPU_NUM × PER_CPU_SIZE (512 KiB each) ]
                   PER_CPU_ARRAY_PTR = __core_end
                   (each slot holds that CPU's boot stack + PerCpu data)
            ──► mem_pool_start() = __core_end + MAX_CPU_NUM * PER_CPU_SIZE
                   [ HV_MEM_POOL_SIZE = 64 MiB dynamic allocation pool ]
            ──► hv_end()         = mem_pool_start + 64 MiB
```

`HV_EXTENDED_SIZE = MAX_CPU_NUM * PER_CPU_SIZE + HV_MEM_POOL_SIZE`, so
`__hv_end == hv_end()`. That constant is exposed to the linker as an assembly
symbol (`consts.rs`, via `global_asm!`) so the `.ld` file can reference a
Rust-defined value.

This is the connection point with the boot code: in `multiboot.S`, CPU 0's boot
stack top is `bsp_boot_stack_top = __core_end + {per_cpu_size}` — the top of the
first 512 KiB slot.

---

## Full picture

```
phys 0x200000 / virt 0xffffff8000200000
│
├─ .text   [.text.header → entry stubs → code]   (MB2 header at the very start)
├─ .rodata
├─ .data   [.data.entry_page_table → ...]
├─ .bss    [.bss.stack → (sbss) zeroed bss (ebss)]
├─ .percpu (VMA = 0, LMA here; CPU_NUM copies)
│
├─ __core_end ───────── end of the ELF image ─────────
│
├─ per-CPU areas   MAX_CPU_NUM × 512 KiB   (stack + PerCpu per core)
├─ mem_pool_start
├─ 64 MiB dynamic memory pool
└─ __hv_end (== hv_end)
```

**Mental model:** the linker script defines the **static image**
(`skernel` → `__core_end`) plus a set of **boundary symbols**; Rust then uses
those symbols to lay out the **dynamic regions** (per-CPU areas and the memory
pool) that follow it in physical RAM.

---

## Symbol reference

| Symbol | Meaning |
|---|---|
| `skernel` / `hv_start()` | start of the image (= `BASE_ADDRESS`) |
| `stext` / `etext` | bounds of `.text` |
| `srodata` / `erodata` | bounds of `.rodata` |
| `sdata` / `edata` | bounds of `.data` |
| `sbss` / `ebss` | bounds of the **zeroed** BSS (excludes `.bss.stack`) |
| `_percpu_start` / `_percpu_end` | load region reserved for `.percpu` copies |
| `ekernel` | end of BSS/percpu (pre-discard) |
| `__core_end` | end of the static image; start of per-CPU areas |
| `mem_pool_start()` | start of the 64 MiB dynamic pool |
| `hv_end()` / `__hv_end` | end of all hypervisor memory |
| `HV_EXTENDED_SIZE` | `MAX_CPU_NUM*PER_CPU_SIZE + HV_MEM_POOL_SIZE` (Rust → linker) |
