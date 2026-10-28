/** Text of the counter example's page. */

const counter = {
  count: 'The count',
  hint: 'One number in the contract’s storage. Anyone can increase it; the page reads it again after every block.',
  increment: 'Add one',
  incrementTitle: 'Add one to the counter',
  add: 'Add',
  amount: 'Amount, 1 to 1000',
  addTitle: (n: string) => `Add ${n} to the counter`,
  badAmount: 'Enter a whole number from 1 to 1000.',
  recent: 'Recent changes',
  changed: (value: string) => `the count became ${value}`,
  howTitle: 'How it works',
  how: [
    '`init` runs once, at deployment, and stores 0 under the key `counter`.',
    '`increment` and `add` read the count, add to it, store it and emit a `changed` event with the caller.',
    '`get` only reads: the page calls it read-only, so reading costs nothing.',
    'Try the Code tab: change what `add` accepts, compile, deploy your version and call it from Methods.',
  ],
};

export default counter;
