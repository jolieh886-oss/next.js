import { Card, Text, Metric, Grid, AreaChart, DonutChart, Table, TableHead, TableRow, TableHeaderCell, TableBody, TableCell, Badge } from "@tremor/react";

export default function Dashboard() {
  return (
    <main className="p-8 bg-gray-50 min-h-screen">
      <h1 className="text-3xl font-bold mb-6 text-gray-800">營運監測看板</h1>
      
      <Grid numItemsLg={2} className="gap-6">
        {/* (1) 營運狀況 */}
        <Card>
          <Text>營運達成率</Text>
          <Metric>92%</Metric>
          <AreaChart
            className="h-44 mt-4"
            data={[{m:"1月",v:80},{m:"2月",v:85},{m:"3月",v:92}]}
            index="m"
            categories={["v"]}
            colors={["blue"]}
          />
        </Card>

        {/* (2) 安全分析 */}
        <Card>
          <Text>風險等級分佈</Text>
          <DonutChart
            className="h-44 mt-4"
            data={[{n:"高",a:2},{n:"中",a:5},{n:"低",a:10}]}
            category="a"
            index="n"
            colors={["red", "orange", "green"]}
          />
        </Card>

        {/* (3) 檢查結果 */}
        <Card className="col-span-2">
          <Text>近期檢查項目</Text>
          <Table className="mt-4">
            <TableHead>
              <TableRow>
                <TableHeaderCell>項目</TableHeaderCell>
                <TableHeaderCell>狀態</TableHeaderCell>
              </TableRow>
            </TableHead>
            <TableBody>
              <TableRow><TableCell>消防安檢</TableCell><TableCell><Badge color="green">合格</Badge></TableCell></TableRow>
              <TableRow><TableCell>電力負載</TableCell><TableCell><Badge color="red">異常</Badge></TableCell></TableRow>
            </TableBody>
          </Table>
        </Card>

        {/* (4) 設施設備 */}
        <Card className="col-span-2">
          <Text>設施設備運作狀態</Text>
          <Metric>42 / 45 運作中</Metric>
          <div className="mt-4 h-2 w-full bg-gray-200 rounded">
            <div className="h-2 bg-green-500 rounded" style={{width: '93%'}}></div>
          </div>
        </Card>
      </Grid>
    </main>
  );
}
