package main

import (
	"context"
	"os"
	"time"

	logging "github.com/ipfs/go-log/v2"
)

func init() {
	setGoDebugEnv()
	logging.SetAllLoggers(logging.LevelInfo)
	err := logging.SetLogLevel("dht", "error")
	checkError(err)
	// Always mute RtRefreshManager because it breaks terminals
	err = logging.SetLogLevel("dht/RtRefreshManager", "fatal")
	checkError(err)
	err = logging.SetLogLevel("net/identify", "error")
	checkError(err)
	err = logging.SetLogLevel("pubsub", "error")
	checkError(err)
	err = logging.SetLogLevel("swarm2", "error")
	checkError(err)
	err = logging.SetLogLevel("f3/sidecar", "debug")
	checkError(err)
	GoF3NodeImpl = &f3Impl{ctx: context.Background()}
}

type f3Impl struct {
	ctx context.Context
}

// The nil checks of the parameters are ommitted because they are passed from Rust code which are not nil.
// The signature pointer types that are generated by rust2go to avoid lifetime issues
func (f3 *f3Impl) run(rpc_endpoint *string, jwt *string, f3_rpc_endpoint *string, initial_power_table *string, bootstrap_epoch *int64, finality *int64, db *string) bool {
	var err error = nil
	const MAX_RETRY int = 5
	nRetry := 0
	for nRetry <= MAX_RETRY {
		err = run(f3.ctx, *rpc_endpoint, *jwt, *f3_rpc_endpoint, *initial_power_table, *bootstrap_epoch, *finality, *db)
		if err != nil {
			nRetry += 1
			logger.Errorf("Unexpected F3 failure, retrying(%d) in 10s... error=%s", nRetry, err)
			time.Sleep(10 * time.Second)
		}
	}
	return err == nil
}

func checkError(err error) {
	if err != nil {
		panic(err)
	}
}

// To avoid potential panics
// See <https://github.com/ChainSafe/forest/pull/4636#issuecomment-2306500753>
func setGoDebugEnv() {
	os.Setenv("GODEBUG", "invalidptr=0,cgocheck=0")
}
