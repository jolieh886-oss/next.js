import './lib'
import '../shared'

// eslint-disable-next-line no-undef
const list = __turbopack_collect__({
  namespace: 'my-test',
})
console.log('first', list)
Promise.all(list.map((item) => item.import())).then((v) =>
  console.log('first', v)
)
export default function page() {
  return <div>hello</div>
}
