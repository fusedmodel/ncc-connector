//go:build !windows

package execrun

import (
	"os/exec"
	"syscall"
)

// setProcAttr 让子进程自成**进程组**：这样取消/超时能整组回收。
//
// 只杀直接子进程是不够的：`sh -c "docker build …"` 被杀掉之后，docker 客户端
// 还活着（甚至继续推送镜像）—— 那等于「取消了但没停」。
func setProcAttr(cmd *exec.Cmd) {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
}

// killGroup 杀整个进程组（负 pid）。
func killGroup(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
	_ = cmd.Process.Kill() // 兜底：万一进程组没建成
}
