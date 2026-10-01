//go:build windows

package execrun

import "os/exec"

// Windows 上没有进程组语义（要 Job Object 才能整树回收）——
// 这里只杀直接子进程，并在文档里如实说明这个差别。
func setProcAttr(cmd *exec.Cmd) {}

func killGroup(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	_ = cmd.Process.Kill()
}
