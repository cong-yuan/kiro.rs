import { useEffect, useState } from 'react'
import { Plus, Save, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { SettingGroup, SettingRow } from '@/components/console/setting-row'
import { usePromptFilterConfig, useSetPromptFilterConfig } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { PromptFilterConfig, PromptFilterRule } from '@/types/api'

const EMPTY: PromptFilterConfig = {
  filterClaudeCode: false,
  filterEnvNoise: false,
  filterStripBoundaries: false,
  rules: [],
}

function newRule(): PromptFilterRule {
  return {
    id: crypto.randomUUID(),
    name: '',
    type: 'lines-containing',
    match: '',
    replace: '',
    enabled: true,
  }
}

export function PromptFilterSection() {
  const { data, isLoading } = usePromptFilterConfig()
  const { mutate, isPending } = useSetPromptFilterConfig()
  const [draft, setDraft] = useState<PromptFilterConfig>(EMPTY)
  const [dirty, setDirty] = useState(false)

  useEffect(() => {
    if (!data) return
    setDraft({ ...data, rules: data.rules.map((rule) => ({ ...rule })) })
    setDirty(false)
  }, [data])

  const patch = (next: Partial<PromptFilterConfig>) => {
    setDraft((current) => ({ ...current, ...next }))
    setDirty(true)
  }
  const patchRule = (index: number, next: Partial<PromptFilterRule>) => {
    setDraft((current) => ({
      ...current,
      rules: current.rules.map((rule, i) => (i === index ? { ...rule, ...next } : rule)),
    }))
    setDirty(true)
  }
  const save = () => {
    mutate(draft, {
      onSuccess: (saved) => {
        setDraft(saved)
        setDirty(false)
        toast.success('System Prompt 过滤配置已生效')
      },
      onError: (error) => toast.error('保存失败：' + extractErrorMessage(error)),
    })
  }

  return (
    <div className="space-y-6">
      <SettingGroup
        title="System Prompt 过滤"
        description="过滤在 Anthropic、OpenAI Chat 和 Responses 请求进入 Kiro 前统一执行。"
      >
        <SettingRow
          label="替换 Claude Code 内置提示词"
          hint="检测到至少两个 Claude Code 特征标记时，替换为精简后端提示词"
        >
          <Switch
            checked={draft.filterClaudeCode}
            disabled={isLoading || isPending}
            onCheckedChange={(value) => patch({ filterClaudeCode: value })}
          />
        </SettingRow>
        <SettingRow
          label="移除环境噪声"
          hint="删除 Environment、auto memory、gitStatus、recent commits 等注入内容"
        >
          <Switch
            checked={draft.filterEnvNoise}
            disabled={isLoading || isPending}
            onCheckedChange={(value) => patch({ filterEnvNoise: value })}
          />
        </SettingRow>
        <SettingRow
          label="移除边界标记"
          hint="删除 SYSTEM PROMPT / END SYSTEM PROMPT 标记行"
        >
          <Switch
            checked={draft.filterStripBoundaries}
            disabled={isLoading || isPending}
            onCheckedChange={(value) => patch({ filterStripBoundaries: value })}
          />
        </SettingRow>
      </SettingGroup>
      <SettingGroup
        title="自定义规则"
        description="正则规则执行查找替换；按行包含规则会删除包含指定文本的整行（忽略大小写）。"
      >
        <div className="space-y-3">
          {draft.rules.map((rule, index) => (
            <div key={rule.id} className="rounded-lg border border-border/60 p-3">
              <div className="flex flex-wrap items-center gap-2">
                <Switch
                  checked={rule.enabled}
                  disabled={isPending}
                  onCheckedChange={(value) => patchRule(index, { enabled: value })}
                />
                <Input
                  value={rule.name}
                  onChange={(event) => patchRule(index, { name: event.target.value })}
                  placeholder="规则名称"
                  className="h-8 min-w-40 flex-1"
                />
                <select
                  value={rule.type}
                  onChange={(event) => patchRule(index, { type: event.target.value as PromptFilterRule['type'] })}
                  className="h-8 rounded-md border border-input bg-background px-2 text-xs"
                >
                  <option value="lines-containing">按行包含</option>
                  <option value="regex">正则替换</option>
                </select>
                <Button
                  size="icon"
                  variant="ghost"
                  title="删除规则"
                  disabled={isPending}
                  onClick={() => patch({ rules: draft.rules.filter((_, i) => i !== index) })}
                >
                  <Trash2 className="h-4 w-4" />
                </Button>
              </div>
              <div className="mt-2 grid gap-2 md:grid-cols-2">
                <Textarea
                  value={rule.match}
                  onChange={(event) => patchRule(index, { match: event.target.value })}
                  placeholder={rule.type === 'regex' ? '正则表达式' : '要删除的行所包含的文本'}
                  className="min-h-20 font-mono text-xs"
                />
                {rule.type === 'regex' && (
                  <Textarea
                    value={rule.replace}
                    onChange={(event) => patchRule(index, { replace: event.target.value })}
                    placeholder="替换内容；留空表示删除匹配"
                    className="min-h-20 font-mono text-xs"
                  />
                )}
              </div>
            </div>
          ))}
          <Button
            size="sm"
            variant="outline"
            disabled={isPending}
            onClick={() => patch({ rules: [...draft.rules, newRule()] })}
          >
            <Plus className="h-3.5 w-3.5" />
            添加规则
          </Button>
        </div>
      </SettingGroup>
      <div className="flex justify-end border-t border-border/60 pt-4">
        <Button onClick={save} disabled={isLoading || isPending || !dirty}>
          <Save className="h-3.5 w-3.5" />
          {isPending ? '保存中…' : '保存并应用'}
        </Button>
      </div>
    </div>
  )
}
