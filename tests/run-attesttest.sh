#!/usr/bin/env bash
# Attestation (DESIGN.md §9.2) end to end: UEFI boot with a software TPM
# (swtpm), dom0 and a guest it starts get reports, tools/l4h-attest.py
# verifies them against the host evidence and makes a credential for the
# attestation key, which dom0 decrypts on a second boot (TIS this time,
# CRB the first).
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
source "$root/tools/guest-env.sh"
out=$root/build/attest
rm -rf "$out/run" && mkdir -p "$out/run"
"$root/tools/build-linux.sh" vmlinux >"$root/build/linux-build.log" 2>&1 \
    || { grep -E "error|undefined" "$root/build/linux-build.log"; exit 1; }
"$l4h_objcopy" --strip-debug "$l4h_linux_out/vmlinux" "$out/run/vmlinux"
"$root/tools/build-l4hctl.sh"
"$root/tools/build-host.sh"
"$root/tools/mkinitramfs.sh" "$out/run/vm.cpio.gz" "$root/tests/guest/init-attestvm" "$l4h_ctl" >/dev/null
nonce=$(head -c 64 /dev/urandom | od -An -tx1 | tr -d ' \n')
cmdline="console=hvc0 attest_nonce=$nonce"
cat > "$out/run/l4h.conf" <<CONF
# tests/run-attesttest.sh
dom0_disks=0
dom0_cmdline="$cmdline"
CONF
# A fresh TPM, so that the EK certificate is the one from this CA.
rm -rf "$out/tpm"
attest=$root/tools/l4h-attest.py
boot() {  # boot <name> <tpm-crb|tpm-tis> [extra initramfs files...]
    local name=$1 dev=$2
    shift 2
    "$root/tools/mkinitramfs.sh" "$out/run/$name.cpio.gz" "$root/tests/guest/init-attest" \
        "$l4h_ctl" "$out/run/vmlinux" "$out/run/vm.cpio.gz" "$@" >/dev/null
    "$root/tools/mkimage.sh" -k "$out/run/vmlinux" -i "$out/run/$name.cpio.gz" -c "$out/run/l4h.conf" \
        "$out/run/$name.raw" >/dev/null
    "$root/tools/swtpm.sh" "$out/tpm"
    local log=$out/$name.log  # build/*/*.log: kept by CI on failure
    timeout "${ATTESTTEST_TIMEOUT:-180}" "$root/tools/run-uefi.sh" "$out/run/$name.raw" \
        -chardev socket,id=tpm,path="$out/tpm/sock" -tpmdev emulator,id=tpm0,chardev=tpm \
        -device "$dev",tpmdev=tpm0 </dev/null >"$log" 2>&1 &
    local qemu=$!
    while kill -0 "$qemu" 2>/dev/null; do
        grep -aq "ATTEST: done\|l4h-root: all guests stopped" "$log" && { sleep 1; kill "$qemu"; break; }
        sleep 1
    done
    wait "$qemu" 2>/dev/null || true
    grep -aE "ATTEST|trustd|TPM|panic" "$log" | cut -c1-160 || true
}
# The hex after "<what> " on a line of a log, as a file: the longest, as
# the serial copy of a guest's console ("[vm1] ...") may be cut short.
field() {  # field <log> <what> <out file>
    grep -a "$2 " "$1" | sed "s/.*$2 \([0-9a-f]*\).*/\1/" | awk '{ print length, $0 }' | sort -n | tail -1 \
        | python3 -c "import sys; sys.stdout.buffer.write(bytes.fromhex(sys.stdin.read().split()[-1]))" > "$3"
    [ -s "$3" ] || { echo "no $2 in $1"; exit 1; }
}
fail() { echo "ATTESTTEST: FAIL: $*"; exit 1; }

boot boot1 tpm-crb
log=$out/boot1.log
field "$log" "ATTEST: dom0 report" "$out/run/dom0.report"
field "$log" "ATTEST: evidence" "$out/run/evidence1"
field "$log" "ATTESTVM: report" "$out/run/vm.report"
grep -aq "ATTEST: tsm provider l4h" "$log" || fail "no configfs-tsm provider"
grep -aq "ATTEST: tsm report matches" "$log" || fail "configfs-tsm report differs"
grep -aq "ATTEST: tsm evidence matches" "$log" || fail "configfs-tsm evidence differs"
dom0_launch=$(python3 "$attest" measure "$out/run/vmlinux" --initrd "$out/run/boot1.cpio.gz" --cmdline "$cmdline")
vm_launch=$(python3 "$attest" measure "$out/run/vmlinux" --initrd "$out/run/vm.cpio.gz" --cmdline "$cmdline")
ca=$out/tpm/ca/issuercert.pem
python3 "$attest" verify "$out/run/dom0.report" "$out/run/evidence1" --data "$nonce" \
    --launch "$dom0_launch" --ek-ca "$ca" > "$out/run/dom0.json" || fail "dom0 report"
python3 "$attest" verify "$out/run/vm.report" "$out/run/evidence1" --data "$nonce" \
    --launch "$vm_launch" --ek-ca "$ca" > "$out/run/vm.json" || fail "guest report"
json() { python3 -c "import json,sys; d=json.load(open(sys.argv[1])); print(eval(sys.argv[2]))" "$@"; }
[ "$(json "$out/run/dom0.json" 'd["guest"]["dom0"]')" = True ] || fail "dom0 not marked dom0"
[ "$(json "$out/run/vm.json" 'd["guest"]["dom0"]')" = False ] || fail "guest marked dom0"
[ "$(json "$out/run/vm.json" 'd["guest"]["ram"]')" = $((64 << 20)) ] || fail "guest RAM"
# The firmware and GRUB measured the boot.
[ "$(json "$out/run/dom0.json" 'd["host"]["pcrs"]["4"]')" != "$(printf '0%.0s' $(seq 64))" ] || fail "PCR 4 empty"
[ "$(json "$out/run/dom0.json" 'd["host"]["pcrs"]["9"]')" != "$(printf '0%.0s' $(seq 64))" ] || fail "PCR 9 empty"
# Tampering is caught.
cp "$out/run/dom0.report" "$out/run/bad.report"
printf '\x00' | dd of="$out/run/bad.report" bs=1 seek=40 conv=notrunc status=none
! python3 "$attest" verify "$out/run/bad.report" "$out/run/evidence1" 2>/dev/null || fail "tampered report verified"
! python3 "$attest" verify "$out/run/vm.report" "$out/run/evidence1" --launch "$dom0_launch" 2>/dev/null \
    || fail "wrong launch digest accepted"
ak=$(json "$out/run/dom0.json" 'd["host"]["ak_name"]')
echo "ATTESTTEST: reports verify; AK $ak"

secret=$(head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n')
python3 "$attest" make-credential "$out/run/evidence1" "$secret" -o "$out/run/cred" || fail "make-credential"
boot boot2 tpm-tis "$out/run/cred"
log=$out/boot2.log
grep -aq "ATTEST: secret $secret" "$log" || fail "credential not activated"
field "$log" "ATTEST: dom0 report" "$out/run/dom0.report2"
field "$log" "ATTEST: evidence" "$out/run/evidence2"
python3 "$attest" verify "$out/run/dom0.report2" "$out/run/evidence2" --data "$nonce" --ak-name "$ak" \
    --ek-ca "$ca" > "$out/run/dom0.json2" || fail "second boot report"
[ "$(json "$out/run/dom0.json2" 'd["host"]["key"]')" != "$(json "$out/run/dom0.json" 'd["host"]["key"]')" ] \
    || fail "same host key on both boots"
echo "ATTESTTEST: PASS"
