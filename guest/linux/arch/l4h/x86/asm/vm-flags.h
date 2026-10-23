/*
 * Copyright (C) 2004 Jeff Dike (jdike@addtoit.com)
 * Copyright 2003 PathScale, Inc.
 * Licensed under the GPL
 */

#ifndef __VM_FLAGS_X86_H
#define __VM_FLAGS_X86_H

/*
 * As on x86: data (brk, bss) and the stack are executable only with
 * READ_IMPLIES_EXEC or an executable PT_GNU_STACK.  l4h enforces NX
 * (seL4_X86_ExecuteNever), so the mm.h default, always executable, would
 * give every process a writable and executable heap.
 */
#define VM_DATA_DEFAULT_FLAGS	VM_DATA_FLAGS_TSK_EXEC

#endif
