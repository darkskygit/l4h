/* SPDX-License-Identifier: GPL-2.0 */
/*
 * Linker script of the l4h guest kernel: a static ELF image whose first
 * byte is guest-physical address 0 (see l4h/abi.h, ram_base_va).
 */
#define RUNTIME_DISCARD_EXIT
#include <asm/vmlinux.lds.h>
#include <asm/page.h>

KERNEL_STACK_SIZE = 4096 * (1 << CONFIG_KERNEL_STACK_ORDER);

OUTPUT_FORMAT(ELF_FORMAT)
OUTPUT_ARCH(ELF_ARCH)
ENTRY(_start)
jiffies = jiffies_64;

SECTIONS
{
  . = START;
  __binary_start = .;
  PROVIDE (__executable_start = .);

  _text = .;
  .head.text : { KEEP(*(.head.text)) }
  INIT_TEXT_SECTION(PAGE_SIZE)

  .text      :
  {
    _stext = .;
    TEXT_TEXT
    SCHED_TEXT
    LOCK_TEXT
    IRQENTRY_TEXT
    SOFTIRQENTRY_TEXT
    *(.fixup)
    *(.gnu.warning)
  }

  #include <asm/common.lds.S>

  . = ALIGN(PAGE_SIZE);
  __init_begin = .;
  init.data : { INIT_DATA }
  __init_end = .;

  .data    :
  {
    INIT_TASK_DATA(KERNEL_STACK_SIZE)
    DATA_DATA
    CONSTRUCTORS
  }
  .data1   : { *(.data1) }
  .ctors   : { *(.ctors) }
  .dtors   : { *(.dtors) }
  .got     : { *(.got.plt) *(.got) }

  /* Static TLS of os-sel4 (libsel4's IPC buffer pointer). */
  . = ALIGN(64);
  .tdata : { __l4h_tls_start = .; *(.tdata .tdata.*) }
  .tbss  : { *(.tbss .tbss.*) *(.tcommon) . = ALIGN(8); __l4h_tls_end = .; }

  _edata  =  .;
  PROVIDE (edata = .);
  . = ALIGN(PAGE_SIZE);
  __bss_start = .;
  PROVIDE(_bss_start = .);
  SBSS(0)
  BSS(0)
  __bss_stop = .;
  . = ALIGN(PAGE_SIZE);
  _end = .;
  PROVIDE (end = .);

  STABS_DEBUG
  DWARF_DEBUG
  MODINFO
  ELF_DETAILS

  DISCARDS
  /DISCARD/ : { *(.eh_frame) *(.note.gnu.property) }
}
