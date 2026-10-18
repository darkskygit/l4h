#!/usr/bin/env bash
# Package a disk image from tools/mkimage.sh for Google Compute Engine and
# print the commands that import it and start a VM.  Nothing is uploaded.
# Usage: gce-image.sh <disk.raw> [image-name]
#
# Supported machine shapes for now: N2/N2D/C2 etc. with
#   --network-interface=nic-type=VIRTIO_NET   (gVNIC is not supported yet)
#   NVMe boot disks (the guest OS feature below makes GCE attach NVMe)
# Hardware virtualization is neither needed nor used.
set -euo pipefail
disk=$1
name=${2:-l4h-$(date +%Y%m%d-%H%M)}
dir=$(dirname "$disk")
tarball=$dir/$name.tar.gz
# GCE requires the member to be named disk.raw.
ln -f "$disk" "$dir/disk.raw" 2>/dev/null || cp --sparse=always "$disk" "$dir/disk.raw"
tar -C "$dir" -Sczf "$tarball" disk.raw
rm -f "$dir/disk.raw"
cat <<CMDS
$tarball: $(du -h "$tarball" | cut -f1)

To deploy (replace BUCKET, PROJECT and ZONE):
  gsutil cp $tarball gs://BUCKET/
  gcloud compute images create $name --project=PROJECT \\
      --source-uri=gs://BUCKET/$name.tar.gz \\
      --guest-os-features=UEFI_COMPATIBLE
  gcloud compute instances create $name --project=PROJECT --zone=ZONE \\
      --machine-type=n2-standard-4 --image=$name \\
      --network-interface=nic-type=VIRTIO_NET \\
      --metadata=serial-port-enable=TRUE
  gcloud compute connect-to-serial-port $name --zone=ZONE
CMDS
