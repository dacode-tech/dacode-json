/* Minimal Cortex-M layout.
 *
 * Nothing here runs. The binary exists only to be linked, so that the
 * generic code in each contender is monomorphised and the dead code is
 * dropped — which is the only point at which "how big is this library"
 * has an answer. The memory sizes are plausible for an STM32F4 and are
 * otherwise arbitrary.
 */

ENTRY(_start)

MEMORY
{
  FLASH (rx)  : ORIGIN = 0x08000000, LENGTH = 1024K
  RAM   (rwx) : ORIGIN = 0x20000000, LENGTH = 128K
}

SECTIONS
{
  .text :
  {
    KEEP(*(.vector_table))
    *(.text .text.*)
  } > FLASH

  .rodata :
  {
    *(.rodata .rodata.*)
  } > FLASH

  .data : AT(ADDR(.rodata) + SIZEOF(.rodata))
  {
    *(.data .data.*)
  } > RAM

  .bss (NOLOAD) :
  {
    *(.bss .bss.*)
    *(COMMON)
  } > RAM

  /* ARM unwind tables. `panic = "abort"` should leave these empty; they
   * are discarded rather than counted so a stray one cannot inflate a
   * number. */
  /DISCARD/ :
  {
    *(.ARM.exidx .ARM.exidx.*)
    *(.ARM.extab .ARM.extab.*)
    *(.comment)
  }
}
