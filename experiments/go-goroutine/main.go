// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
//
// This source code is licensed under the BSD-style license found in the
// LICENSE file in the root directory of this source tree.

// goroutine-channel-order exposes scheduling-dependent channel receive order.
// NONDET_SOURCE: Go goroutine scheduling.
package main

import (
	"crypto/sha256"
	"encoding/binary"
	"fmt"
	"runtime"
	"strings"
	"sync"
)

const workers = 32

func main() {
	runtime.GOMAXPROCS(8)

	start := make(chan struct{})
	results := make(chan int, workers)
	var ready sync.WaitGroup
	ready.Add(workers)

	for id := 0; id < workers; id++ {
		go func() {
			ready.Done()
			<-start
			runtime.Gosched()
			results <- id
		}()
	}

	ready.Wait()
	close(start)

	order := make([]int, 0, workers)
	hashInput := make([]byte, workers*4)
	labels := make([]string, 0, workers)
	for index := 0; index < workers; index++ {
		id := <-results
		order = append(order, id)
		binary.LittleEndian.PutUint32(hashInput[index*4:], uint32(id))
		labels = append(labels, fmt.Sprintf("%02d", id))
	}

	digest := sha256.Sum256(hashInput)
	fmt.Printf(
		"program=goroutine-channel-order go=%s workers=%d order=%s sha256=%x\n",
		runtime.Version(), workers, strings.Join(labels, ","), digest,
	)
}
