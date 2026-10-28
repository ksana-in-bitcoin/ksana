import type { Messages } from '..';

const counter: Messages['counter'] = {
  count: '当前计数',
  hint: '合约存储里的一个数。任何人都可以增加它；每出一个块，页面都会重新读取。',
  increment: '加一',
  incrementTitle: '给计数器加一',
  add: '增加',
  amount: '数量，1 到 1000',
  addTitle: (n) => `给计数器增加 ${n}`,
  badAmount: '请输入 1 到 1000 之间的整数。',
  recent: '最近的变化',
  changed: (value) => `计数变为 ${value}`,
  howTitle: '原理',
  how: [
    '`init` 只在部署时运行一次，在键 `counter` 下存入 0。',
    '`increment` 和 `add` 读出计数、加上数量、存回去，并发出带调用者的 `changed` 事件。',
    '`get` 只读取：页面以只读方式调用它，读取不花钱。',
    '试试“代码”页：修改 `add` 接受的范围，编译，部署你的版本，再到“方法”页调用它。',
  ],
};

export default counter;
