# Helpers for the watch programs. They reproduce the arithmetic and the text of watch.ps1, so the POSIX
# watch and the PowerShell watch write the same timeline and the same summary.

# Rounds to one decimal place, halves to even, as .NET's Math.Round(x, 1) does.
def round1:
  (. * 10) as $v
  | ($v | floor) as $f
  | ($v - $f) as $d
  | (if $d > 0.5 then $f + 1
     elif $d < 0.5 then $f
     elif ($f % 2) == 0 then $f
     else $f + 1 end) / 10;

# Seconds from the origin to a time in ms, one decimal. Zero until the order is posted, as Get-Elapsed.
def elapsed_from($origin; $ms):
  if $origin == null then 0 else ((($ms - $origin) / 1000) | round1) end;

# Pads a string on the left (lpad) or on the right (rpad) to $n characters.
def lpad($n): (" " * ([$n - length, 0] | max)) + .;
def rpad($n): . + (" " * ([$n - length, 0] | max));

# A UTC time in ms as .NET's "o" format writes it: seven fraction digits and a Z.
def iso7_from_ms:
  (. / 1000 | floor | todate | sub("Z$"; "")) as $base
  | (. % 1000 | tostring | ("00" + .)[-3:]) as $frac
  | "\($base).\($frac)0000Z";

# Milliseconds since the epoch of a UTC time written as norm7 writes it.
def ms_from_iso7:
  capture("^(?<base>[^.]+)\\.(?<frac>[0-9]+)Z$")
  | ((.base + "Z") | fromdateiso8601) * 1000 + ((.frac + "000")[0:3] | tonumber);

# A UTC time as the worker log wrote it, padded to seven fraction digits as .NET does.
def norm7:
  capture("^(?<base>[^.]+)(\\.(?<frac>[0-9]*))?Z$")
  | "\(.base).\(((.frac // "") + "0000000")[0:7])Z";
