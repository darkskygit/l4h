/* SPDX-License-Identifier: GPL-2.0 */
#ifndef __ASM_L4H_VDSO_VSYSCALL_H
#define __ASM_L4H_VDSO_VSYSCALL_H

/* Data pages mapped just below the vDSO text (only the time page is used). */
#define __VDSO_PAGES	4

#ifndef __ASSEMBLER__

#include <vdso/datapage.h>
#include <asm-generic/vdso/vsyscall.h>

#endif /* !__ASSEMBLER__ */

#endif
