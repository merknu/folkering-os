.SUFFIXES:

override SOURCE_DATE_EPOCH := 1768412449
export SOURCE_DATE_EPOCH

override SOURCE_DATE_EPOCH_TOUCH := 202601141840

override PACKAGE_TARNAME := limine
override PACKAGE_VERSION := 10.6.3
override DIST_OUTPUT := $(PACKAGE_TARNAME)-$(PACKAGE_VERSION)

override prefix := /usr/local
override exec_prefix := ${prefix}

override bindir := ${exec_prefix}/bin
override datarootdir := ${prefix}/share
override mandir := ${datarootdir}/man
override docdir := ${datarootdir}/doc/${PACKAGE_TARNAME}

override BUILDDIR := /work/limine
override BINDIR := $(BUILDDIR)/bin

override SRCDIR := /work/limine

override SPACE := $(subst ,, )
override COMMA := ,

override MKESCAPE = $(subst $(SPACE),\ ,$(1))
override SHESCAPE = $(subst ','\'',$(1))
override NASMESCAPE = $(subst ','"'$(COMMA) \"'\"$(COMMA) '"',$(1))

override BUILD_BIOS := 
override BUILD_UEFI_X86_64 := 
override BUILD_UEFI_IA32 := 
override BUILD_UEFI_AARCH64 := 
override BUILD_UEFI_RISCV64 := 
override BUILD_UEFI_LOONGARCH64 := 
override BUILD_UEFI_CD := no
override BUILD_BIOS_PXE := no
override BUILD_BIOS_CD := no

INSTALL := /usr/bin/install -c
INSTALL_PROGRAM := ${INSTALL}
INSTALL_DATA := ${INSTALL} -m 644
STRIP := strip

MKDIR_P := /usr/bin/mkdir -p
export MKDIR_P
GREP := /usr/bin/grep
export GREP
SED := /usr/bin/sed
export SED
AWK := mawk
export AWK

CC := gcc

CPPFLAGS := 
CFLAGS := -g -O2 -pipe
LDFLAGS := 
LIBS := 

CC_FOR_TARGET := clang
export CC_FOR_TARGET
LD_FOR_TARGET := ld.lld
export LD_FOR_TARGET
OBJCOPY_FOR_TARGET := llvm-objcopy
export OBJCOPY_FOR_TARGET
OBJDUMP_FOR_TARGET := llvm-objdump
export OBJDUMP_FOR_TARGET
READELF_FOR_TARGET := llvm-readelf
export READELF_FOR_TARGET

override WERROR_FLAG := -Wno-error
export WERROR_FLAG

CFLAGS_FOR_TARGET :=  -g -O2 -pipe
export CFLAGS_FOR_TARGET
CPPFLAGS_FOR_TARGET := 
export CPPFLAGS_FOR_TARGET
LDFLAGS_FOR_TARGET := 
export LDFLAGS_FOR_TARGET
NASMFLAGS_FOR_TARGET := -g
export NASMFLAGS_FOR_TARGET

override STAGE1_FILES := $(shell find '$(call SHESCAPE,$(SRCDIR))/stage1' -type f -name '*.asm' | LC_ALL=C sort)

.PHONY: all
all: $(call MKESCAPE,$(BINDIR))/Makefile
	$(MAKE) all1

.PHONY: all1
all1: $(BUILD_UEFI_X86_64) $(BUILD_UEFI_IA32) $(BUILD_UEFI_AARCH64) $(BUILD_UEFI_RISCV64) $(BUILD_UEFI_LOONGARCH64) $(BUILD_BIOS)
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/limine'
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin'

$(call MKESCAPE,$(BINDIR))/limine-bios-hdd.h: $(call MKESCAPE,$(BINDIR))/limine-bios-hdd.bin
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cd '$(call SHESCAPE,$(BINDIR))' && '$(call SHESCAPE,$(SRCDIR))/host/hgen.sh' >limine-bios-hdd.h

override LIMINE_NO_BIOS :=
ifneq ($(BUILD_BIOS),limine-bios)
override LIMINE_NO_BIOS := -DLIMINE_NO_BIOS
endif

$(call MKESCAPE,$(BINDIR))/limine: $(call MKESCAPE,$(BINDIR))/Makefile $(call MKESCAPE,$(SRCDIR))/host/limine.c $(if $(filter $(BUILD_BIOS),limine-bios),$(call MKESCAPE,$(BINDIR))/limine-bios-hdd.h)
	$(SED) 's/%VERSION%/10.6.3/g;s/%COPYRIGHT%/Copyright (C) 2019-2026 Mintsuki and contributors./g' <'$(call SHESCAPE,$(SRCDIR))/host/limine.c' >'$(call SHESCAPE,$(BINDIR))/limine.c'
	$(MAKE) -C '$(call SHESCAPE,$(BINDIR))' limine \
		CC="$(CC)" \
		CFLAGS="$(CFLAGS) -Wall -Wextra $(WERROR_FLAG)" \
		CPPFLAGS='$(CPPFLAGS) $(LIMINE_NO_BIOS) -DLIMINE_DATADIR=\"$(call SHESCAPE,$(datarootdir))/limine\"' \
		LDFLAGS="$(LDFLAGS)" \
		LIBS="$(LIBS)"

$(call MKESCAPE,$(BINDIR))/Makefile: $(call MKESCAPE,$(SRCDIR))/host/host.mk $(call MKESCAPE,$(SRCDIR))/host/.gitignore
	mkdir -p '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(SRCDIR))/host/host.mk' '$(call SHESCAPE,$(BINDIR))/Makefile'
	cp '$(call SHESCAPE,$(SRCDIR))/host/.gitignore' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine
limine:
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/limine'

.PHONY: clean
clean: limine-bios-clean limine-uefi-ia32-clean limine-uefi-x86-64-clean limine-uefi-aarch64-clean limine-uefi-riscv64-clean limine-uefi-loongarch64-clean
	rm -rf '$(call SHESCAPE,$(BINDIR))' '$(call SHESCAPE,$(BUILDDIR))/stage1.stamp'

.PHONY: install
install: all
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(docdir))'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/COPYING' '$(call SHESCAPE,$(DESTDIR)$(docdir))/'
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(docdir))/LICENSES'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/LICENSES/LicenseRef-scancode-bsd-no-disclaimer-unmodified.txt' '$(call SHESCAPE,$(DESTDIR)$(docdir))/LICENSES/'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/3RDPARTY.md' '$(call SHESCAPE,$(DESTDIR)$(docdir))/'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/CONFIG.md' '$(call SHESCAPE,$(DESTDIR)$(docdir))/'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/FAQ.md' '$(call SHESCAPE,$(DESTDIR)$(docdir))/'
	$(INSTALL_DATA) '$(call SHESCAPE,$(SRCDIR))/USAGE.md' '$(call SHESCAPE,$(DESTDIR)$(docdir))/'
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(mandir))/man1'
	$(INSTALL_DATA) '$(call SHESCAPE,$(BUILDDIR))/man/man1/limine.1' '$(call SHESCAPE,$(DESTDIR)$(mandir))/man1/'
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(datarootdir))'
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine'
ifeq ($(BUILD_BIOS),limine-bios)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/limine-bios.sys' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifneq ($(BUILD_BIOS_CD),no)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/limine-bios-cd.bin' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifneq ($(BUILD_UEFI_CD),no)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifneq ($(BUILD_BIOS_PXE),no)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/limine-bios-pxe.bin' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifeq ($(BUILD_UEFI_AARCH64),limine-uefi-aarch64)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/BOOTAA64.EFI' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifeq ($(BUILD_UEFI_RISCV64),limine-uefi-riscv64)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/BOOTRISCV64.EFI' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifeq ($(BUILD_UEFI_LOONGARCH64),limine-uefi-loongarch64)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/BOOTLOONGARCH64.EFI' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifeq ($(BUILD_UEFI_X86_64),limine-uefi-x86-64)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/BOOTX64.EFI' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
ifeq ($(BUILD_UEFI_IA32),limine-uefi-ia32)
	$(INSTALL_DATA) '$(call SHESCAPE,$(BINDIR))/BOOTIA32.EFI' '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine/'
endif
	$(INSTALL) -d '$(call SHESCAPE,$(DESTDIR)$(bindir))'
	$(INSTALL_PROGRAM) '$(call SHESCAPE,$(BINDIR))/limine' '$(call SHESCAPE,$(DESTDIR)$(bindir))/'

.PHONY: install-strip
install-strip: install
	$(STRIP) '$(call SHESCAPE,$(DESTDIR)$(bindir))/limine'

.PHONY: uninstall
uninstall:
	rm -f '$(call SHESCAPE,$(DESTDIR)$(docdir))/COPYING'
	rm -rf '$(call SHESCAPE,$(DESTDIR)$(docdir))/LICENSES'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(docdir))/3RDPARTY.md'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(docdir))/CONFIG.md'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(docdir))/FAQ.md'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(docdir))/USAGE.md'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(mandir))/man1/limine.1'
	rm -f '$(call SHESCAPE,$(DESTDIR)$(bindir))/limine'
	rm -rf '$(call SHESCAPE,$(DESTDIR)$(datarootdir))/limine'

$(call MKESCAPE,$(BUILDDIR))/stage1.stamp: $(STAGE1_FILES) $(call MKESCAPE,$(BUILDDIR))/decompressor-build/decompressor.bin $(call MKESCAPE,$(BUILDDIR))/common-bios/stage2.bin.gz
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cd '$(call SHESCAPE,$(SRCDIR))/stage1/hdd' && nasm bootsect.asm -Wall -w-unknown-warning -w-reloc $(WERROR_FLAG) -fbin -DBUILDDIR="'"'$(call NASMESCAPE,$(BUILDDIR))'"'" -o '$(call SHESCAPE,$(BINDIR))/limine-bios-hdd.bin'
ifneq ($(BUILD_BIOS_CD),no)
	cd '$(call SHESCAPE,$(SRCDIR))/stage1/cd'  && nasm bootsect.asm -Wall -w-unknown-warning -w-reloc $(WERROR_FLAG) -fbin -DBUILDDIR="'"'$(call NASMESCAPE,$(BUILDDIR))'"'" -o '$(call SHESCAPE,$(BINDIR))/limine-bios-cd.bin'
endif
ifneq ($(BUILD_BIOS_PXE),no)
	cd '$(call SHESCAPE,$(SRCDIR))/stage1/pxe' && nasm bootsect.asm -Wall -w-unknown-warning -w-reloc $(WERROR_FLAG) -fbin -DBUILDDIR="'"'$(call NASMESCAPE,$(BUILDDIR))'"'" -o '$(call SHESCAPE,$(BINDIR))/limine-bios-pxe.bin'
endif
	cp '$(call SHESCAPE,$(BUILDDIR))/common-bios/limine-bios.sys' '$(call SHESCAPE,$(BINDIR))/'
	touch '$(call SHESCAPE,$(BUILDDIR))/stage1.stamp'

.PHONY: limine-bios
limine-bios: common-bios decompressor
	$(MAKE) '$(call SHESCAPE,$(BUILDDIR))/stage1.stamp'

$(call MKESCAPE,$(BINDIR))/limine-uefi-cd.bin: $(if $(BUILD_UEFI_IA32),$(call MKESCAPE,$(BUILDDIR))/common-uefi-ia32/BOOTIA32.EFI) $(if $(BUILD_UEFI_X86_64),$(call MKESCAPE,$(BUILDDIR))/common-uefi-x86-64/BOOTX64.EFI) $(if $(BUILD_UEFI_AARCH64),$(call MKESCAPE,$(BUILDDIR))/common-uefi-aarch64/BOOTAA64.EFI) $(if $(BUILD_UEFI_RISCV64),$(call MKESCAPE,$(BUILDDIR))/common-uefi-riscv64/BOOTRISCV64.EFI) $(if $(BUILD_UEFI_LOONGARCH64),$(call MKESCAPE,$(BUILDDIR))/common-uefi-loongarch64/BOOTLOONGARCH64.EFI)
ifneq ($(BUILD_UEFI_CD),no)
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	rm -f '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin'
	dd if=/dev/zero of='$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin' bs=512 count=5760 2>/dev/null
	mformat -i '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin' -f 2880 -N 12345678 ::
	LIMINE_UEFI_CD_TMP="$$(mktemp -d)"; \
		mkdir -p "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT; \
		cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-aarch64/BOOTAA64.EFI' "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT/ 2>/dev/null; \
		cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-riscv64/BOOTRISCV64.EFI' "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT/ 2>/dev/null; \
		cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-loongarch64/BOOTLOONGARCH64.EFI' "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT/ 2>/dev/null; \
		cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-x86-64/BOOTX64.EFI' "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT/ 2>/dev/null; \
		cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-ia32/BOOTIA32.EFI' "$$LIMINE_UEFI_CD_TMP"/EFI/BOOT/ 2>/dev/null; \
		find "$$LIMINE_UEFI_CD_TMP" -exec touch -t $(SOURCE_DATE_EPOCH_TOUCH) '{}' + && \
		mcopy -D o -s -m -i '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin' "$$LIMINE_UEFI_CD_TMP"/EFI :: && \
		rm -rf "$$LIMINE_UEFI_CD_TMP"
endif

.PHONY: limine-uefi-cd
limine-uefi-cd:
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/limine-uefi-cd.bin'

$(call MKESCAPE,$(BINDIR))/BOOTX64.EFI: $(call MKESCAPE,$(BUILDDIR))/common-uefi-x86-64/BOOTX64.EFI
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-x86-64/BOOTX64.EFI' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine-uefi-x86-64
limine-uefi-x86-64:
	$(MAKE) common-uefi-x86-64
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/BOOTX64.EFI'

$(call MKESCAPE,$(BINDIR))/BOOTIA32.EFI: $(call MKESCAPE,$(BUILDDIR))/common-uefi-ia32/BOOTIA32.EFI
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-ia32/BOOTIA32.EFI' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine-uefi-ia32
limine-uefi-ia32:
	$(MAKE) common-uefi-ia32
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/BOOTIA32.EFI'

$(call MKESCAPE,$(BINDIR))/BOOTAA64.EFI: $(call MKESCAPE,$(BUILDDIR))/common-uefi-aarch64/BOOTAA64.EFI
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-aarch64/BOOTAA64.EFI' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine-uefi-aarch64
limine-uefi-aarch64:
	$(MAKE) common-uefi-aarch64
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/BOOTAA64.EFI'

$(call MKESCAPE,$(BINDIR))/BOOTRISCV64.EFI: $(call MKESCAPE,$(BUILDDIR))/common-uefi-riscv64/BOOTRISCV64.EFI
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-riscv64/BOOTRISCV64.EFI' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine-uefi-riscv64
limine-uefi-riscv64:
	$(MAKE) common-uefi-riscv64
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/BOOTRISCV64.EFI'

$(call MKESCAPE,$(BINDIR))/BOOTLOONGARCH64.EFI: $(call MKESCAPE,$(BUILDDIR))/common-uefi-loongarch64/BOOTLOONGARCH64.EFI
	$(MKDIR_P) '$(call SHESCAPE,$(BINDIR))'
	cp '$(call SHESCAPE,$(BUILDDIR))/common-uefi-loongarch64/BOOTLOONGARCH64.EFI' '$(call SHESCAPE,$(BINDIR))/'

.PHONY: limine-uefi-loongarch64
limine-uefi-loongarch64:
	$(MAKE) common-uefi-loongarch64
	$(MAKE) '$(call SHESCAPE,$(BINDIR))/BOOTLOONGARCH64.EFI'

.PHONY: limine-bios-clean
limine-bios-clean: common-bios-clean decompressor-clean

.PHONY: limine-uefi-x86-64-clean
limine-uefi-x86-64-clean: common-uefi-x86-64-clean

.PHONY: limine-uefi-ia32-clean
limine-uefi-ia32-clean: common-uefi-ia32-clean

.PHONY: limine-uefi-aarch64-clean
limine-uefi-aarch64-clean: common-uefi-aarch64-clean

.PHONY: limine-uefi-riscv64-clean
limine-uefi-riscv64-clean: common-uefi-riscv64-clean

.PHONY: limine-uefi-loongarch64-clean
limine-uefi-loongarch64-clean: common-uefi-loongarch64-clean

.PHONY: dist
dist:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)"
	$(MKDIR_P) '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)"
	cp -r '$(call SHESCAPE,$(SRCDIR))'/.git '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)"/
	cd '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)" && git checkout .
	cd '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)" && ./bootstrap
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/flanterm/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/flanterm/.gitignore"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/freestnd-c-hdrs/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/limine-protocol/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/picoefi/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/picoefi/.gitignore"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/cc-runtime"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/libfdt/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/tinf"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/common/lib/stb_image.h.nopatch"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/.git"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/.gitignore"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/.forgejo"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/README.md"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/autom4te.cache"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/test"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/test.mk"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/logo.png"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/screenshot.png"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/bochsrc"
	echo "$(PACKAGE_VERSION)" > '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)/version"
	cd '$(call SHESCAPE,$(BUILDDIR))' && tar -cf "$(DIST_OUTPUT).tar" "$(DIST_OUTPUT)"
	cd '$(call SHESCAPE,$(BUILDDIR))' && gzip < "$(DIST_OUTPUT).tar" > "$(DIST_OUTPUT).tar.gz"
	cd '$(call SHESCAPE,$(BUILDDIR))' && ( bzip2 < "$(DIST_OUTPUT).tar" > "$(DIST_OUTPUT).tar.bz2" || rm -f "$(DIST_OUTPUT).tar.bz2" )
	cd '$(call SHESCAPE,$(BUILDDIR))' && ( xz < "$(DIST_OUTPUT).tar" > "$(DIST_OUTPUT).tar.xz" || rm -f "$(DIST_OUTPUT).tar.xz" )
	cd '$(call SHESCAPE,$(BUILDDIR))' && rm "$(DIST_OUTPUT).tar"
	rm -rf '$(call SHESCAPE,$(BUILDDIR))'/"$(DIST_OUTPUT)"

.PHONY: distclean
distclean: clean
	rm -rf edk2-ovmf config.log config.status GNUmakefile config.h man/man1/limine.1

.PHONY: maintainer-clean
maintainer-clean: distclean
	cd '$(call SHESCAPE,$(SRCDIR))' && rm -rf flanterm common/lib/stb_image.h.nopatch common/lib/stb_image.h decompressor/tinf tinf libfdt freestnd-c-hdrs cc-runtime common/cc-runtime.s2.c decompressor/cc-runtime.c limine-protocol picoefi configure timestamps build-aux *'~' autom4te.cache aclocal.m4 *.tar*

.PHONY: common-uefi-x86-64
common-uefi-x86-64:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=uefi-x86-64 \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-uefi-x86-64'

.PHONY: common-uefi-x86-64-clean
common-uefi-x86-64-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-uefi-x86-64'

.PHONY: common-uefi-aarch64
common-uefi-aarch64:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=uefi-aarch64 \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-uefi-aarch64'

.PHONY: common-uefi-aarch64-clean
common-uefi-aarch64-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-uefi-aarch64'

.PHONY: common-uefi-riscv64
common-uefi-riscv64:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=uefi-riscv64 \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-uefi-riscv64'

.PHONY: common-uefi-riscv64-clean
common-uefi-riscv64-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-uefi-riscv64'

.PHONY: common-uefi-loongarch64
common-uefi-loongarch64:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=uefi-loongarch64 \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-uefi-loongarch64'

.PHONY: common-uefi-loongarch64-clean
common-uefi-loongarch64-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-uefi-loongarch64'

.PHONY: common-uefi-ia32
common-uefi-ia32:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=uefi-ia32 \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-uefi-ia32'

.PHONY: common-uefi-ia32-clean
common-uefi-ia32-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-uefi-ia32'

.PHONY: common-bios
common-bios:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/common' -f common.mk \
		TARGET=bios \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/common-bios'

.PHONY: common-bios-clean
common-bios-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/common-bios'

.PHONY: decompressor
decompressor:
	$(MAKE) -C '$(call SHESCAPE,$(SRCDIR))/decompressor' -f decompressor.mk \
		BUILDDIR='$(call SHESCAPE,$(BUILDDIR))/decompressor-build'

.PHONY: decompressor-clean
decompressor-clean:
	rm -rf '$(call SHESCAPE,$(BUILDDIR))/decompressor-build'

-include test.mk
