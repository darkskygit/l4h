# Freestanding test guest for the l4h guest ABI (see main.c).
ROOT := $(abspath ../..)
SEL4 := $(ROOT)/build/sel4/libsel4/include
OUT  := $(ROOT)/build/testguest
CFLAGS := -O2 -g -ffreestanding -fno-stack-protector -fno-pic -fno-pie -mno-red-zone \
	  -mno-sse -mno-mmx -mno-avx -mcmodel=large -nostdinc -isystem $(shell $(CC) -print-file-name=include) \
	  -I$(SEL4) -I$(ROOT)/include -Wall -Wno-builtin-declaration-mismatch

$(OUT)/testguest.elf: main.c start.S linker.ld $(ROOT)/include/l4h/abi.h
	mkdir -p $(OUT)
	$(CC) $(CFLAGS) -c main.c -o $(OUT)/main.o
	$(CC) $(CFLAGS) -c start.S -o $(OUT)/start.o
	$(LD) -static -nostdlib -T linker.ld -z noexecstack -z max-page-size=4096 $(OUT)/start.o $(OUT)/main.o -o $@

clean:
	rm -rf $(OUT)
