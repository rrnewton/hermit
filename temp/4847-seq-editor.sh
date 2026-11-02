#!/bin/sh
sed -i \
 -e 's/^pick 9a0b75d072/drop 9a0b75d072/' \
 -e 's/^pick 5930d4d0c5/drop 5930d4d0c5/' \
 -e 's/^pick c740cadb59/drop c740cadb59/' \
 -e 's/^pick 7bcb3a229a/drop 7bcb3a229a/' \
 -e 's/^pick a87b7df6e7/drop a87b7df6e7/' \
 -e 's/^pick 5337ebf676/drop 5337ebf676/' \
 -e 's/^pick ead69c0b94/drop ead69c0b94/' \
 -e 's/^pick bd772a16e8/drop bd772a16e8/' \
 "$1"
