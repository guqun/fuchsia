# zx_pci_init

## NAME

<!-- Contents of this heading updated by update-docs-from-fidl, do not edit. -->

This function is obsolete and should not be used.

## SYNOPSIS

<!-- Contents of this heading updated by update-docs-from-fidl, do not edit. -->

```c
#include <zircon/syscalls.h>

zx_status_t zx_pci_init(zx_handle_t handle,
                        const zx_pci_init_arg_t* init_buf,
                        uint32_t len);
```

## DESCRIPTION

This function is obsolete and should not be used. Drivers should instead use the PCI protocol
Typically, you obtain this in your **bind()** function through **device_get_protocol()**.

## RIGHTS

<!-- Contents of this heading updated by update-docs-from-fidl, do not edit. -->

*handle* must have resource kind **ZX_RSRC_KIND_ROOT**.

## RETURN VALUE

TODO(fxbug.dev/32938)

## ERRORS

TODO(fxbug.dev/32938)

## SEE ALSO


TODO(fxbug.dev/32938)
