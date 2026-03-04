import '../shared'

// eslint-disable-next-line no-undef
const list = __turbopack_collect__({
  namespace: 'my-test',
})
console.log('second', list)
Promise.all(list.map((item) => item.import())).then((v) =>
  console.log('second', v)
)

export default function page() {
  return <div>hello</div>
}
