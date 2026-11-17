package termd_test

import (
	"os"
	"testing"

	"github.com/tikimcfee/gomuxai/internal/termdtest"
)

func TestMain(m *testing.M) {
	os.Exit(termdtest.Run(m))
}
