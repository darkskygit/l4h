/* SPDX-License-Identifier: GPL-2.0 */
SECTIONS {
 . = SIZEOF_HEADERS;
 .text 0x100 : { *(.text) } :text
 .dynsym : { *(.dynsym) }
 .dynstr : { *(.dynstr) }
 .hash : { *(.hash) }
 .gnu.hash : { *(.gnu.hash) }
 .gnu.version : { *(.gnu.version) }
 .gnu.version_d : { *(.gnu.version_d) }
 .dynamic : { *(.dynamic) } :text :dynamic
 /DISCARD/ : { *(.note*) *(.comment) *(.eh_frame) }
}
PHDRS { text PT_LOAD FILEHDR PHDRS FLAGS(5); dynamic PT_DYNAMIC FLAGS(4); }
VERSION { LINUX_2.6.39 { global: __kernel_rt_sigreturn; local: *; }; }
