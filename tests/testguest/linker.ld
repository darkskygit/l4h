ENTRY(_start)
SECTIONS
{
	. = 0x60000000;
	.text : { *(.text.start) *(.text.user) *(.text*) }
	.rodata : { *(.rodata*) }
	.data : { *(.data*) }
	.tdata : { *(.tdata*) }
	.tbss : { *(.tbss*) }
	.bss : { *(.bss*) *(COMMON) }
	/DISCARD/ : { *(.note*) *(.comment) *(.eh_frame*) }
}
