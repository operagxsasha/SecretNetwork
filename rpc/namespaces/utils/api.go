package utils

import (
	"fmt"
	"strings"

	sdk "github.com/cosmos/cosmos-sdk/types"
	"github.com/ethereum/go-ethereum/common"
	"github.com/scrtlabs/SecretNetwork/types"
)

type API struct{}

// NewAPI creates an instance of the utils API.
func NewAPI() *API {
	return &API{}
}

// ConvertAddress converts provided address from bech32 format to hex
// and vice versa
func (a *API) ConvertAddress(address string) (string, error) {
	switch {
	case common.IsHexAddress(address):
		addrBytes := common.HexToAddress(address).Bytes()
		convertedAddr := sdk.AccAddress(addrBytes)
		return convertedAddr.String(), nil
	case strings.HasPrefix(address, types.Bech32PrefixAccAddr):
		addrBytes, _ := sdk.AccAddressFromBech32(address)
		convertedAddr := common.BytesToAddress(addrBytes)
		return convertedAddr.String(), nil
	default:
		return "", fmt.Errorf("expected a valid hex or bech32 address")
	}
}
