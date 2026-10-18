/* SPDX-License-Identifier: GPL-2.0 */
.text
.globl __kernel_rt_sigreturn
.type __kernel_rt_sigreturn, %function
__kernel_rt_sigreturn:
 mov x8, #139
 svc #0
.size __kernel_rt_sigreturn, . - __kernel_rt_sigreturn
