#!/bin/sh
# PHP matching engine benchmark. Сборки нет: PHP интерпретируется, OPcache JIT
# компилирует горячий цикл на лету (прогрев — по протоколу автора).
# Аргументы пробрасываются как есть: --iters N --seed HEX [--json].
#
# JIT-режим 1054 = tracing JIT БЕЗ распределения регистров (R=0). В PHP 8.5.10
# (arm64) режимы с регистрами (1254/tracing, 1205/function с R=2) на этом коде
# выдают неверные результаты на части seed (проверялось перебором seed против
# интерпретатора), поэтому выбран самый быстрый из корректных на всех seed.
set -eu
cd "$(dirname "$0")"
PHP=/opt/homebrew/opt/php/bin/php
exec "$PHP" \
  -d memory_limit=256M \
  -d opcache.enable_cli=1 \
  -d opcache.jit_buffer_size=64M \
  -d opcache.jit=1054 \
  -d opcache.jit_hot_loop=1 \
  engine.php "$@"
