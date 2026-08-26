'use strict';

/*
 * Inline glossary behind the `?` affordances in both Bridge windows.
 *
 * tools/logic2-bridge/test/delivery.test.cjs extracts the keys below with a
 * line-oriented regex and asserts they exactly match the set of `data-hint`
 * attributes across index.html and status-panel.html. Keep every entry on its
 * own line in the `  'key': {` form so a reworded hint cannot silently orphan
 * an affordance in either direction.
 */
const PXLOGIC_HINTS = {
  'threshold': {
    title: '电压判断阈值',
    body: 'PXLogic 硬件比较器的判定电平，不是目标电路的供电电压。信号高于它记为 1，低于记为 0。必须按实际探头、目标和信号质量来选：同一个 3.3 V 目标，短探头下 1.65 V 可能很好，长引线或带振铃的信号往往需要另选。仅凭目标的标称逻辑电平推不出可靠阈值。',
  },
  'threshold-reference': {
    title: '参考起点',
    body: '下拉里的每一项只是常见逻辑电平的中点，是调试的起点而不是答案。选定后仍需用已知协议的解码结果验证：能看到边沿不等于采到的数据正确。验证通过后再勾选下方的确认框。',
  },
  'firmware-version': {
    title: 'MCU 固件版本',
    body: 'Bridge 随包携带 PXView 1.4.5 起的四个 MCU 固件镜像，默认始终是最新那个。切到较旧版本会在下次启动时改写 PXLogic 的 MCU 闪存并让设备重新枚举；该操作无法从 Bridge 内部撤销，只能再次切换版本重写。四个版本共用同一套 FPGA 位流。',
  },
  'logic-sample-rate': {
    title: 'Logic 采样率',
    body: 'Logic 2 请求的采样率，完全由 Logic 2 的采集设置决定。Bridge 不修改它，只负责按这个请求去挑选 PXLogic 能实际提供的硬件档位。',
  },
  'effective-sample-rate': {
    title: '硬件有效率',
    body: 'PXLogic 硬件实际使用的采样率。它常与 Logic 采样率不同，因为硬件只支持 PXView 定义的若干「通道数 + 最高采样率」组合，Bridge 会挑能覆盖请求的最接近档位。两者不同是正常现象，不代表出错。',
  },
  'pxview-mode': {
    title: 'PXView 模式',
    body: 'Bridge 依据当前通道数与采样率选中的 PXView 硬件档位，例如「Stream: Use 8 Channels (Max 250MHz)」。档位同时决定可用通道数与硬件能达到的最高采样率，因此改变启用通道数可能会换档。',
  },
  'data-loss': {
    title: '下溢与丢弃',
    body: '下溢指 Logic 2 要数据时注入队列是空的，通常是上游供数跟不上，波形会出现间隙，但已经到达的数据仍然可信。丢弃指数据已经从硬件取到却没能送进 Logic 2，那部分样本永久丢失。丢弃不为零时本次采集结果不可信。',
  },
  'graphserver-identity': {
    title: 'GraphServer 与会话设备',
    body: 'Bridge 把 PXLogic 伪装成 Logic Pro 16，所以 Logic 2 里的会话设备显示为 Demo Logic Pro 16，而真实数据来自 PXLogic。GraphServer 指纹是所选 Logic 2 内部采集服务的二进制标识，Bridge 用它判断能否安全地注入数据。',
  },
};

(() => {
  const state = { button: null, popover: null };

  function ensurePopover() {
    if (state.popover) return state.popover;
    const popover = document.createElement('div');
    popover.className = 'hint-popover';
    popover.id = 'hint-popover';
    popover.setAttribute('role', 'tooltip');
    popover.hidden = true;
    popover.innerHTML = '<strong class="hint-title"></strong><p class="hint-body"></p>';
    document.body.appendChild(popover);
    state.popover = popover;
    return popover;
  }

  function close() {
    if (!state.button) return;
    state.button.setAttribute('aria-expanded', 'false');
    state.button.removeAttribute('aria-describedby');
    state.button = null;
    if (state.popover) state.popover.hidden = true;
  }

  // Prefer below the trigger, flip above when there is no room, and always keep
  // the bubble inside the viewport. The status panel is only 340 px wide, so
  // horizontal clamping matters as much as the vertical flip.
  function place(button) {
    const popover = state.popover;
    const margin = 8;
    popover.style.left = '0px';
    popover.style.top = '0px';
    popover.hidden = false;
    const anchor = button.getBoundingClientRect();
    const box = popover.getBoundingClientRect();
    let left = anchor.left;
    if (left + box.width > window.innerWidth - margin) {
      left = window.innerWidth - margin - box.width;
    }
    left = Math.max(margin, left);
    let top = anchor.bottom + 6;
    if (top + box.height > window.innerHeight - margin) {
      const above = anchor.top - 6 - box.height;
      top = above >= margin ? above : Math.max(margin, window.innerHeight - margin - box.height);
    }
    popover.style.left = `${Math.round(left)}px`;
    popover.style.top = `${Math.round(top)}px`;
  }

  function open(button) {
    const hint = PXLOGIC_HINTS[button.dataset.hint];
    if (!hint) return;
    close();
    const popover = ensurePopover();
    popover.querySelector('.hint-title').textContent = hint.title;
    popover.querySelector('.hint-body').textContent = hint.body;
    state.button = button;
    button.setAttribute('aria-expanded', 'true');
    button.setAttribute('aria-describedby', popover.id);
    place(button);
  }

  document.addEventListener('click', event => {
    const trigger = event.target.closest?.('.hint-button');
    if (trigger) {
      event.preventDefault();
      if (state.button === trigger) close();
      else open(trigger);
      return;
    }
    if (state.button && !state.popover.contains(event.target)) close();
  });

  document.addEventListener('keydown', event => {
    if (event.key === 'Escape' && state.button) {
      event.preventDefault();
      const trigger = state.button;
      close();
      // Focus never enters the bubble, but a mouse user may have moved it away.
      trigger.focus();
    }
  });

  // Repositioning a fixed bubble during resize is not worth the complexity for
  // a non-modal hint; dismissing keeps it from drifting away from its trigger.
  window.addEventListener('resize', close);

  document.addEventListener('DOMContentLoaded', () => {
    for (const button of document.querySelectorAll('.hint-button[data-hint]')) {
      button.setAttribute('aria-expanded', 'false');
    }
  });
})();
