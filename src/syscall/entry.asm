; ============================================================================
; Golem Linux — gkern
; src/syscall/entry.asm
;
; x86_64 SYSCALL/SYSRET entry and exit path.
;
; Linux x86_64 syscall ABI (which gkern matches bit-for-bit):
;
;   user register   purpose
;   -------------   -------------------------------------------------------
;   rax             syscall number          (also the return value on exit)
;   rdi             arg0
;   rsi             arg1
;   rdx             arg2
;   r10             arg3   (NOT rcx — rcx is clobbered by SYSCALL)
;   r8              arg4
;   r9              arg5
;   rcx             clobbered: SYSCALL saves user RIP here
;   r11             clobbered: SYSCALL saves user RFLAGS here
;
; All other registers must be preserved across the boundary.
;
; SYSCALL hardware behavior (Intel SDM Vol. 2B, AMD APM Vol. 3):
;   - Loads RIP from IA32_LSTAR MSR
;   - Loads CS/SS from IA32_STAR[63:48] (kernel selectors)
;   - Saves user RIP -> RCX, user RFLAGS -> R11
;   - Masks RFLAGS with IA32_FMASK (we clear IF to disable interrupts on entry)
;   - Does NOT switch stacks — we must do that manually using swapgs + GS:0
;
; SYSRET reverses this: RIP <- RCX, RFLAGS <- R11, CS/SS from IA32_STAR[63:48]+16.
;
; The per-CPU kernel stack pointer is stored at GS:[0] (set up by the
; per-CPU init code owned by the memory/CPU-init agent). We use SWAPGS to
; flip GS between user and kernel views on each transition.
; ============================================================================

bits 64
default rel

global syscall_entry
global syscall_init_msrs

extern syscall_dispatch          ; Rust: fn(nr,a0,a1,a2,a3,a4,a5) -> i64

; ----------------------------------------------------------------------------
; Per-CPU layout offsets at GS:[...]
;   0  : kernel_rsp     — top of this CPU's kernel stack
;   8  : user_rsp_save  — scratch slot to stash user RSP during entry
; ----------------------------------------------------------------------------
%define PCPU_KERNEL_RSP    0
%define PCPU_USER_RSP_SAVE 8

; ----------------------------------------------------------------------------
; MSR numbers
; ----------------------------------------------------------------------------
%define MSR_EFER   0xC0000080
%define MSR_STAR   0xC0000081
%define MSR_LSTAR  0xC0000082
%define MSR_FMASK  0xC0000084

%define EFER_SCE   0x1            ; SYSCALL Enable bit

; Selectors loaded by SYSCALL/SYSRET. Layout per AMD APM:
;   STAR[47:32] = kernel CS  (SS = CS+8)
;   STAR[63:48] = user   CS  (SS = CS+8) for compat; 64-bit user CS = base+16
; gkern GDT (owned by another agent) is laid out:
;   0x08 = kernel code, 0x10 = kernel data,
;   0x18 = user code32 (unused), 0x20 = user data, 0x28 = user code64
; That gives the STAR value below.
%define STAR_VALUE 0x0023000800000000

; RFLAGS bits we mask off on syscall entry. Clearing IF disables interrupts
; the instant we enter the kernel; clearing DF normalizes string ops; clearing
; TF prevents single-step traps from leaking into kernel mode.
%define FMASK_VALUE 0x0000000000047700  ; IF|DF|TF|IOPL|NT|AC

section .text

; ----------------------------------------------------------------------------
; syscall_entry — invoked directly by the CPU on the SYSCALL instruction.
;
; On entry:
;   - We are in CPL 0, on the *user* stack.
;   - Interrupts are disabled (FMASK cleared IF).
;   - rcx = user RIP, r11 = user RFLAGS.
;   - GS still points at the user view.
; ----------------------------------------------------------------------------
syscall_entry:
    swapgs                                  ; GS -> kernel per-CPU area
    mov     [gs:PCPU_USER_RSP_SAVE], rsp    ; stash user RSP
    mov     rsp, [gs:PCPU_KERNEL_RSP]       ; load this CPU's kernel stack

    ; Build a minimal trap frame on the kernel stack. Order is chosen so a
    ; future ptrace/signal path can read it as a struct without re-shuffling.
    push    qword [gs:PCPU_USER_RSP_SAVE]   ; user RSP
    push    r11                             ; user RFLAGS
    push    rcx                             ; user RIP
    push    rax                             ; syscall nr (for restart)

    ; Preserve callee-clobbered user registers the SysV ABI lets Rust trash.
    ; We save them here so SYSRET restores the user's complete state.
    push    rdi
    push    rsi
    push    rdx
    push    r10
    push    r8
    push    r9
    push    rbx
    push    rbp
    push    r12
    push    r13
    push    r14
    push    r15

    ; Translate Linux ABI -> SysV C ABI for the call into Rust:
    ;   Linux:   nr=rax, a0=rdi, a1=rsi, a2=rdx, a3=r10, a4=r8, a5=r9
    ;   SysV C:  a0=rdi, a1=rsi, a2=rdx, a3=rcx, a4=r8,  a5=r9
    ; So we need: rdi<-rax, rsi<-rdi, rdx<-rsi, rcx<-rdx, r8 stays via r10->r8? No.
    ; The clean mapping is:
    ;   new rdi (a0=nr)   <- rax
    ;   new rsi (a1=arg0) <- old rdi
    ;   new rdx (a2=arg1) <- old rsi
    ;   new rcx (a3=arg2) <- old rdx
    ;   new r8  (a4=arg3) <- r10
    ;   new r9  (a5=arg4) <- r8
    ;   stack   (a6=arg5) <- r9
    ; Rust signature: syscall_dispatch(nr, a0..a5).
    sub     rsp, 8                          ; 16-byte align before call
    push    r9                              ; arg5 on stack (7th C arg)
    mov     r9, r8                          ; arg4
    mov     r8, r10                         ; arg3
    mov     rcx, rdx                        ; arg2
    mov     rdx, rsi                        ; arg1
    mov     rsi, rdi                        ; arg0
    mov     rdi, rax                        ; nr

    call    syscall_dispatch

    add     rsp, 16                         ; drop arg5 + alignment pad

    ; rax now holds the return value from Rust. Restore callee state.
    pop     r15
    pop     r14
    pop     r13
    pop     r12
    pop     rbp
    pop     rbx
    pop     r9
    pop     r8
    pop     r10
    pop     rdx
    pop     rsi
    pop     rdi

    add     rsp, 8                          ; drop saved syscall-nr slot
    pop     rcx                             ; user RIP -> rcx for SYSRET
    pop     r11                             ; user RFLAGS -> r11 for SYSRET
    pop     rsp                             ; restore user RSP

    swapgs                                  ; GS -> user view
    o64 sysret                              ; return to userspace (64-bit form)

; ----------------------------------------------------------------------------
; syscall_init_msrs — called once per CPU during bring-up.
;
; Programs the four MSRs the CPU consults on SYSCALL/SYSRET. Other agents
; must have already loaded a valid GDT and configured the per-CPU GS base
; before invoking this routine.
; ----------------------------------------------------------------------------
syscall_init_msrs:
    ; EFER.SCE = 1 (enable SYSCALL/SYSRET in 64-bit mode)
    mov     ecx, MSR_EFER
    rdmsr
    or      eax, EFER_SCE
    wrmsr

    ; STAR — kernel/user segment selectors used by SYSCALL/SYSRET
    mov     ecx, MSR_STAR
    mov     eax, 0
    mov     edx, (STAR_VALUE >> 32) & 0xFFFFFFFF
    wrmsr

    ; LSTAR — RIP loaded on SYSCALL
    mov     ecx, MSR_LSTAR
    lea     rax, [syscall_entry]
    mov     rdx, rax
    shr     rdx, 32
    wrmsr

    ; FMASK — bits cleared from RFLAGS on SYSCALL entry
    mov     ecx, MSR_FMASK
    mov     eax, FMASK_VALUE & 0xFFFFFFFF
    mov     edx, 0
    wrmsr

    ret
