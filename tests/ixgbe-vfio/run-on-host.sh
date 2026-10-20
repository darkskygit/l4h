#!/usr/bin/env bash
# Run ixgbe-vfio on a Linux host against one of its own ports (root).  The
# port leaves the kernel driver for the run, so run this detached when it
# is the port you are logged in through:
#   systemd-run --unit=ixgbe-vfio ./run-on-host.sh 0000:01:00.1 IP GW
# The log is in /root/ixgbe-vfio.log; the port returns to ixgbe after.
set -u
bdf=$1; ip=$2; gw=$3
here=$(cd "$(dirname "$0")" && pwd)
log=/root/ixgbe-vfio.log
modprobe vfio-pci
echo "$bdf" > /sys/bus/pci/drivers/ixgbe/unbind
echo vfio-pci > "/sys/bus/pci/devices/$bdf/driver_override"
echo "$bdf" > /sys/bus/pci/drivers_probe
{
    echo "== without offloads"
    timeout 60 "$here/ixgbe-vfio" "$bdf" "$ip" "$gw"; echo "exit $?"
    echo "== with offloads"
    timeout 90 "$here/ixgbe-vfio" "$bdf" "$ip" "$gw" --offload; echo "exit $?"
} > "$log" 2>&1
echo "$bdf" > /sys/bus/pci/drivers/vfio-pci/unbind
echo > "/sys/bus/pci/devices/$bdf/driver_override"
echo "$bdf" > /sys/bus/pci/drivers/ixgbe/bind
sleep 5
systemctl restart systemd-networkd
