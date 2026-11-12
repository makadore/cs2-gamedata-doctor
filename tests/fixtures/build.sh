#!/bin/bash
# Rebuild the fixture shared objects from src/*.cpp. The committed old.so/new.so are the output of
# this script; tests do not need a compiler. -fno-toplevel-reorder keeps functions in definition
# order (the fixtures rely on it). Hidden visibility mimics a stripped game binary:
# no vtable or typeinfo symbols, only the RTTI data itself.
set -e
cd "$(dirname "$0")"
for v in old new; do
  g++ -std=c++17 -shared -fPIC -O1 -fno-toplevel-reorder -fvisibility=hidden -Wl,-z,relro -o "$v.so" "src/$v.cpp"
  strip --strip-all "$v.so"
done
ls -la old.so new.so
