package main

import (
	"context"

	"github.com/filecoin-project/go-f3"
	"github.com/filecoin-project/go-f3/certs"
	"github.com/filecoin-project/go-f3/gpbft"
	"github.com/filecoin-project/go-f3/manifest"
	"github.com/filecoin-project/go-state-types/crypto"
	"github.com/libp2p/go-libp2p/core/peer"
)

type F3Api struct {
	GetRawNetworkName        func(context.Context) (string, error)
	GetTipsetByEpoch         func(context.Context, int64) (TipSet, error)
	GetTipset                func(context.Context, gpbft.TipSetKey) (TipSet, error)
	GetHead                  func(context.Context) (TipSet, error)
	GetParent                func(context.Context, gpbft.TipSetKey) (TipSet, error)
	GetPowerTable            func(context.Context, gpbft.TipSetKey) (gpbft.PowerEntries, error)
	ProtectPeer              func(context.Context, peer.ID) (bool, error)
	GetParticipatingMinerIDs func(context.Context) ([]uint64, error)
	SignMessage              func(context.Context, []byte, []byte) (*crypto.Signature, error)
	Finalize                 func(context.Context, gpbft.TipSetKey) error
}

type FilecoinApi struct {
	Version        func(context.Context) (VersionInfo, error)
	NetAddrsListen func(context.Context) (peer.AddrInfo, error)
}

type VersionInfo struct {
	APIVersion int
	BlockDelay int
	Version    string
}

type F3ServerHandler struct {
	f3 *f3.F3
}

func (h *F3ServerHandler) F3GetCertificate(ctx context.Context, instance uint64) (*certs.FinalityCertificate, error) {
	return h.f3.GetCert(ctx, instance)
}

func (h *F3ServerHandler) F3GetLatestCertificate(ctx context.Context) (*certs.FinalityCertificate, error) {
	return h.f3.GetLatestCert(ctx)
}

func (h *F3ServerHandler) F3GetF3PowerTable(ctx context.Context, tsk []byte) (gpbft.PowerEntries, error) {
	return h.f3.GetPowerTable(ctx, tsk)
}

// F3GetF3PowerTableByInstance retrieves the power table for a specific consensus instance.
// It returns the power entries associated with the given instance number.
//
// Parameters:
//   - ctx: The context for the operation
//   - instance: The consensus instance number to retrieve the power table for
//
// Returns:
//   - PowerEntries: The power distribution table for the specified instance
//   - error: Any error encountered during retrieval
func (h *F3ServerHandler) F3GetF3PowerTableByInstance(ctx context.Context, instance uint64) (gpbft.PowerEntries, error) {
	return h.f3.GetPowerTableByInstance(ctx, instance)
}

func (h *F3ServerHandler) F3IsRunning(_ context.Context) bool {
	return h.f3.IsRunning()
}

func (h *F3ServerHandler) F3GetProgress(_ context.Context) gpbft.InstanceProgress {
	return h.f3.Progress()
}

func (h *F3ServerHandler) F3GetManifest(ctx context.Context) manifest.Manifest {
	m := h.f3.Manifest()
	if !isCidDefined(m.InitialPowerTable) {
		if cert0, err := h.f3.GetCert(ctx, 0); err == nil {
			m.InitialPowerTable = cert0.ECChain.Base().PowerTable
		}
	}
	return m
}
